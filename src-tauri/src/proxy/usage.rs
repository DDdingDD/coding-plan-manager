use serde::Serialize;

/// 从响应体中解析出的 token 用量
#[derive(Debug, Default, Clone, Serialize)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    /// Anthropic prompt caching：命中缓存读取的输入 token（不含在 input_tokens 中）
    pub cache_read_tokens: i64,
    /// Anthropic prompt caching：本次新写入缓存的输入 token
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
}

/// normalize_usage 从单个 usage 对象中解析出的可选字段
struct RawUsage {
    prompt: Option<i64>,
    completion: Option<i64>,
    total: Option<i64>,
    cache_read: Option<i64>,
    cache_creation: Option<i64>,
}

/// 兼容 OpenAI（prompt_tokens/completion_tokens/total_tokens）
/// 与 Anthropic（input_tokens/output_tokens）两种 usage 格式
fn normalize_usage(v: &serde_json::Value) -> RawUsage {
    // OpenAI / Anthropic message_delta：usage 在顶层
    // Anthropic message_start：usage 嵌套在 message.usage
    let usage = v
        .get("usage")
        .or_else(|| v.get("message").and_then(|m| m.get("usage")));
    let usage = match usage {
        Some(u) if u.is_object() => u,
        _ => {
            return RawUsage {
                prompt: None,
                completion: None,
                total: None,
                cache_read: None,
                cache_creation: None,
            }
        }
    };
    let prompt = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(|x| x.as_i64());
    let completion = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(|x| x.as_i64());
    let total = usage.get("total_tokens").and_then(|x| x.as_i64());
    // Anthropic 的缓存字段是 input_tokens 之外的独立部分；
    // OpenAI 的 prompt_tokens_details.cached_tokens 是 prompt_tokens 的子集（已含在 total 中），不解析以免重复计数
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(|x| x.as_i64());
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(|x| x.as_i64());
    RawUsage {
        prompt,
        completion,
        total,
        cache_read,
        cache_creation,
    }
}

/// 解析非流式 JSON 响应体
pub fn parse_json_usage(body: &str) -> Usage {
    let mut u = Usage::default();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        let r = normalize_usage(&v);
        u.prompt_tokens = r.prompt.unwrap_or(0);
        u.completion_tokens = r.completion.unwrap_or(0);
        u.cache_read_tokens = r.cache_read.unwrap_or(0);
        u.cache_creation_tokens = r.cache_creation.unwrap_or(0);
        u.total_tokens = r
            .total
            .unwrap_or(u.prompt_tokens + u.completion_tokens + u.cache_read_tokens + u.cache_creation_tokens);
    }
    u
}

/// SSE 用量增量解析器：边转发边按行解析，只保留不完整行的残余。
/// 流式转发不能依赖落库缓冲解析 usage（2MB 截断会丢掉流尾部的
/// message_delta / 最终 chunk，导致 output_tokens 严重少计），故在转发途中增量解析。
#[derive(Default)]
pub struct SseUsageTracker {
    /// 跨 chunk 的不完整行残余
    pending: Vec<u8>,
    prompt: Option<i64>,
    completion: Option<i64>,
    total: Option<i64>,
    cache_read: Option<i64>,
    cache_creation: Option<i64>,
}

impl SseUsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一个上游 chunk：切出完整行立即解析，残余留给下次
    pub fn feed(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=pos).collect();
            self.parse_line(&line);
        }
        // 防护：异常流长时间无换行时丢弃残余，避免无限增长
        if self.pending.len() > 4 * 1024 * 1024 {
            self.pending.clear();
        }
    }

    /// 流结束：解析残余的最后一行（可能无换行结尾），产出合计用量
    pub fn finish(mut self) -> Usage {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.parse_line(&line);
        }
        let prompt = self.prompt.unwrap_or(0);
        let completion = self.completion.unwrap_or(0);
        let cache_read = self.cache_read.unwrap_or(0);
        let cache_creation = self.cache_creation.unwrap_or(0);
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
            total_tokens: self
                .total
                .unwrap_or(prompt + completion + cache_read + cache_creation),
        }
    }

    fn parse_line(&mut self, line: &[u8]) {
        let line = String::from_utf8_lossy(line);
        let data = match line.strip_prefix("data:") {
            Some(d) => d.trim(),
            None => return,
        };
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
            let r = normalize_usage(&v);
            if r.prompt.is_some() {
                self.prompt = r.prompt;
            }
            if r.completion.is_some() {
                self.completion = r.completion;
            }
            if r.total.is_some() {
                self.total = r.total;
            }
            if r.cache_read.is_some() {
                self.cache_read = r.cache_read;
            }
            if r.cache_creation.is_some() {
                self.cache_creation = r.cache_creation;
            }
        }
    }
}

/// 解析 SSE 流文本：逐个解析 `data:` 行，后面的 usage 覆盖前面的
/// （Anthropic 的 message_start 带 input_tokens 与缓存字段，message_delta 持续更新 output_tokens；
///   OpenAI 在最后一个 chunk 带 usage）
pub fn parse_sse_usage(body: &str) -> Usage {
    let mut tracker = SseUsageTracker::new();
    tracker.feed(body.as_bytes());
    tracker.finish()
}

/// 按响应 Content-Type 选择解析方式
pub fn parse_usage(content_type: &str, body: &str) -> Usage {
    if body.is_empty() {
        return Usage::default();
    }
    if content_type.contains("text/event-stream") {
        parse_sse_usage(body)
    } else {
        parse_json_usage(body)
    }
}

/// 从 JSON 文本顶层提取 "model" 字段（SSE 文本逐行尝试 data: 行）
fn model_from_text(body: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
            return Some(m.to_string());
        }
    }
    // SSE：Anthropic message_start 的 model 嵌套在 message.model，逐行找顶层/嵌套 model
    for line in body.lines() {
        let data = match line.strip_prefix("data:") {
            Some(d) => d.trim(),
            None => continue,
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
            if let Some(m) = v
                .get("model")
                .or_else(|| v.get("message").and_then(|m| m.get("model")))
                .and_then(|m| m.as_str())
            {
                return Some(m.to_string());
            }
        }
    }
    None
}

/// 提取模型名：优先请求体的 "model"（Anthropic/OpenAI 请求均携带），
/// 回退响应体（非流式 JSON 顶层或 SSE 的 message_start）
pub fn extract_model(req_body: &str, resp_body: &str) -> String {
    model_from_text(req_body)
        .or_else(|| model_from_text(resp_body))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_json() {
        let body = r#"{"id":"x","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":34,"total_tokens":46}}"#;
        let u = parse_json_usage(body);
        assert_eq!(u.prompt_tokens, 12);
        assert_eq!(u.completion_tokens, 34);
        assert_eq!(u.total_tokens, 46);
    }

    #[test]
    fn anthropic_json() {
        let body = r#"{"id":"msg","usage":{"input_tokens":7,"output_tokens":9}}"#;
        let u = parse_json_usage(body);
        assert_eq!(u.prompt_tokens, 7);
        assert_eq!(u.completion_tokens, 9);
        assert_eq!(u.total_tokens, 16);
    }

    #[test]
    fn anthropic_json_with_cache() {
        // prompt caching：input_tokens 不含缓存部分，total 需按原始口径合计四项
        let body = r#"{"id":"msg","usage":{"input_tokens":1200,"cache_creation_input_tokens":8000,"cache_read_input_tokens":91000,"output_tokens":500}}"#;
        let u = parse_json_usage(body);
        assert_eq!(u.prompt_tokens, 1200);
        assert_eq!(u.completion_tokens, 500);
        assert_eq!(u.cache_creation_tokens, 8000);
        assert_eq!(u.cache_read_tokens, 91000);
        assert_eq!(u.total_tokens, 1200 + 8000 + 91000 + 500);
    }

    #[test]
    fn no_usage() {
        let u = parse_json_usage(r#"{"ok":true}"#);
        assert_eq!(u.total_tokens, 0);
        let u = parse_json_usage("not json");
        assert_eq!(u.total_tokens, 0);
    }

    #[test]
    fn anthropic_sse() {
        let body = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":2}}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":57}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";
        let u = parse_sse_usage(body);
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 57);
        assert_eq!(u.total_tokens, 157);
    }

    #[test]
    fn anthropic_sse_with_cache() {
        // message_start 带缓存字段，message_delta 更新 output_tokens，total 合计四项
        let body = "\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1000,\"cache_read_input_tokens\":50000,\"cache_creation_input_tokens\":2000,\"output_tokens\":1}}}\n\
\n\
data: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":300}}\n";
        let u = parse_sse_usage(body);
        assert_eq!(u.prompt_tokens, 1000);
        assert_eq!(u.completion_tokens, 300);
        assert_eq!(u.cache_read_tokens, 50000);
        assert_eq!(u.cache_creation_tokens, 2000);
        assert_eq!(u.total_tokens, 1000 + 50000 + 2000 + 300);
    }

    #[test]
    fn openai_sse() {
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\
\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":20,\"total_tokens\":28}}\n\
\n\
data: [DONE]\n";
        let u = parse_sse_usage(body);
        assert_eq!(u.prompt_tokens, 8);
        assert_eq!(u.completion_tokens, 20);
        assert_eq!(u.total_tokens, 28);
    }

    #[test]
    fn sse_tracker_incremental_across_chunks() {
        // 逐字节喂入，模拟任意分块边界（含多字节字符被撕开的情况）
        let full = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":30,\"output_tokens\":1}}}\n\
                    \n\
                    data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"汉字\"}}\n\
                    \n\
                    data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n";
        let mut t = SseUsageTracker::new();
        for b in full.as_bytes() {
            t.feed(std::slice::from_ref(b));
        }
        let u = t.finish();
        assert_eq!(u.prompt_tokens, 30);
        assert_eq!(u.completion_tokens, 9);
        assert_eq!(u.total_tokens, 39);
    }

    #[test]
    fn sse_tracker_unterminated_last_line() {
        // 最后一行无换行符，finish 时也应解析
        let mut t = SseUsageTracker::new();
        t.feed(b"data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6,\"total_tokens\":11}}");
        let u = t.finish();
        assert_eq!(u.total_tokens, 11);
    }

    #[test]
    fn dispatch_by_content_type() {
        let sse = "data: {\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}";
        assert_eq!(parse_usage("text/event-stream", sse).total_tokens, 7);
        assert_eq!(parse_usage("application/json", sse).total_tokens, 0); // SSE 文本不是合法整体 JSON
        assert_eq!(parse_usage("application/json", "{}").total_tokens, 0);
        assert_eq!(parse_usage("text/event-stream", "").total_tokens, 0);
    }

    #[test]
    fn model_extraction() {
        // 请求体优先
        assert_eq!(
            extract_model(r#"{"model":"glm-4.7","messages":[]}"#, r#"{"model":"other"}"#),
            "glm-4.7"
        );
        // 回退响应体（JSON 顶层）
        assert_eq!(extract_model("", r#"{"model":"claude-sonnet-5"}"#), "claude-sonnet-5");
        // 回退响应体（SSE message_start 嵌套）
        let sse = "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-haiku-4.5\"}}";
        assert_eq!(extract_model("", sse), "claude-haiku-4.5");
        // 都没有
        assert_eq!(extract_model("", ""), "");
    }
}
