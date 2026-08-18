use crate::db::{self, NewMessage};
use crate::proxy::strategy;
use crate::proxy::usage;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use rusqlite::Connection;
use serde_json::json;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio_stream::wrappers::ReceiverStream;

/// 请求体最大读取 10MB
const MAX_REQUEST_BODY: usize = 10 * 1024 * 1024;
/// 落库的请求/响应体最大长度 2MB（超出截断并标记）
const MAX_STORED_BODY: usize = 2 * 1024 * 1024;
/// 非流式响应最大读取 64MB（防护）
const MAX_RESPONSE_BODY: usize = 64 * 1024 * 1024;

/// 转发到上游时跳过的请求头（跳段头 + 原鉴权头 + 编码协商头）
const SKIP_REQ_HEADERS: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-authorization",
    "proxy-authenticate",
    "te",
    "trailer",
    "upgrade",
    "accept-encoding",
];

/// 回传给客户端时跳过的响应头
const SKIP_RESP_HEADERS: &[&str] =
    &["content-length", "transfer-encoding", "connection", "keep-alive"];

/// 每个聚合器服务一份，随服务启动创建。
/// app 为 None 时不向前端推送事件（集成测试场景）。
pub struct ProxyShared {
    pub aggregator_id: i64,
    pub db: Arc<Mutex<Connection>>,
    pub client: reqwest::Client,
    pub app: Option<tauri::AppHandle>,
}

/// 兜底路由：任意 method + 任意 path 都进入这里（透明代理）
pub async fn proxy(State(shared): State<Arc<ProxyShared>>, req: Request) -> Response {
    handle(shared, req).await
}

async fn handle(shared: Arc<ProxyShared>, req: Request) -> Response {
    let started = Instant::now();
    let method = req.method().as_str().to_string();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let headers = req.headers().clone();

    // ---- 1. 读取聚合器最新配置 + 鉴权 ----
    let agg_cfg = {
        let conn = lock_db(&shared);
        db::get_aggregator(&conn, shared.aggregator_id).ok().flatten()
    };
    let agg_cfg = match agg_cfg {
        Some(a) => a,
        None => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "聚合器配置读取失败",
            )
        }
    };

    let presented = presented_token(&headers);
    if presented.as_deref() != Some(agg_cfg.auth_token.as_str()) {
        store_message(
            &shared,
            shared.aggregator_id,
            None,
            None,
            &method,
            &path,
            401,
            "",
            "鉴权失败：无效的 AUTH_TOKEN",
            String::new(),
            &usage::Usage::default(),
            started,
        )
        .await;
        return error_response(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "无效的 AUTH_TOKEN",
        );
    }

    // ---- 2. 读取请求体 ----
    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_REQUEST_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("请求体读取失败: {e}"),
            )
        }
    };
    let req_body_stored = cap_store(&String::from_utf8_lossy(&body_bytes));

    // ---- 3. 转发策略：选择一个 coding plan ----
    let pick = {
        let conn = lock_db(&shared);
        strategy::pick_plan(&conn, &agg_cfg)
    };
    let pick = match pick {
        Ok(Some(p)) => p,
        Ok(None) => {
            store_message(
                &shared,
                agg_cfg.id,
                None,
                None,
                &method,
                &path,
                503,
                &req_body_stored,
                "没有可用的 Coding Plan（未绑定或已全部禁用）",
                usage::extract_model(&req_body_stored, ""),
                &usage::Usage::default(),
                started,
            )
            .await;
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded_error",
                "没有可用的 Coding Plan（未绑定或已全部禁用）",
            );
        }
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                &format!("转发策略执行失败: {e}"),
            )
        }
    };

    // ---- 4. 构造上游请求（替换鉴权头为计划的 AUTH_TOKEN）----
    let base = pick.plan.base_url.trim_end_matches('/');
    let url = format!("{base}{path}");

    let mut fwd_headers = reqwest::header::HeaderMap::new();
    for (name, value) in &headers {
        if SKIP_REQ_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            // append 保住重复头的多个值（insert 会只剩最后一个）
            fwd_headers.append(n, v);
        }
    }
    let plan_token = pick.plan.auth_token.clone();
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {plan_token}")) {
        fwd_headers.insert(reqwest::header::AUTHORIZATION, v);
    }
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&plan_token) {
        fwd_headers.insert(reqwest::header::HeaderName::from_static("x-api-key"), v);
    }

    let method_up = match reqwest::Method::from_bytes(method.as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            return error_response(StatusCode::METHOD_NOT_ALLOWED, "invalid_request_error", "不支持的 HTTP 方法")
        }
    };

    let mut builder = shared.client.request(method_up, &url).headers(fwd_headers);
    if !body_bytes.is_empty() || matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
        builder = builder.body(body_bytes);
    }

    let upstream_resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("上游请求失败: {e}");
            store_message(
                &shared,
                agg_cfg.id,
                Some(pick.plan.id),
                None,
                &method,
                &path,
                502,
                &req_body_stored,
                &msg,
                usage::extract_model(&req_body_stored, ""),
                &usage::Usage::default(),
                started,
            )
            .await;
            return error_response(StatusCode::BAD_GATEWAY, "api_error", &msg);
        }
    };

    // ---- 5. 回传响应（SSE 流式透传 / JSON 整体回传）----
    let status_code = upstream_resp.status().as_u16() as i64;
    let content_type = upstream_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let mut resp_builder = Response::builder().status(
        StatusCode::from_u16(status_code as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
    );
    for (name, value) in upstream_resp.headers().iter() {
        if SKIP_RESP_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            resp_builder = resp_builder.header(n, v);
        }
    }

    let agg_id = agg_cfg.id;
    let plan_id = pick.plan.id;
    let binding_id = pick.binding_id;

    if content_type.contains("text/event-stream") {
        // 流式：边转发给客户端边累积，结束后解析用量并落库
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
        let shared2 = shared.clone();
        let method2 = method.clone();
        let path2 = path.clone();
        let req_stored = req_body_stored.clone();
        let ct = content_type.clone();
        tauri::async_runtime::spawn(async move {
            let mut stream = upstream_resp.bytes_stream();
            // 累积原始字节（超过落库上限即截断），结束后一次性 lossy 解码：
            // 逐 chunk 解码会把跨分块的多字节字符撕裂成替换符
            let mut buf: Vec<u8> = Vec::new();
            let mut truncated = false;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        if buf.len() < MAX_STORED_BODY {
                            let remain = MAX_STORED_BODY - buf.len();
                            if bytes.len() > remain {
                                buf.extend_from_slice(&bytes[..remain]);
                                truncated = true;
                            } else {
                                buf.extend_from_slice(&bytes);
                            }
                        } else {
                            truncated = true;
                        }
                        // 客户端断开则停止读取
                        if tx.send(Ok(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(std::io::Error::other(e))).await;
                        break;
                    }
                }
            }
            drop(tx);

            let collected = decode_sse_stored(&buf, truncated);
            let u = usage::parse_usage(&ct, &collected);
            let model = usage::extract_model(&req_stored, &collected);
            store_message(
                &shared2,
                agg_id,
                Some(plan_id),
                Some(binding_id),
                &method2,
                &path2,
                status_code,
                &req_stored,
                &collected,
                model,
                &u,
                started,
            )
            .await;
        });

        let body = Body::from_stream(ReceiverStream::new(rx));
        resp_builder.body(body).unwrap_or_else(|_| Response::new(Body::empty()))
    } else {
        // 非流式：整体读取后回传
        let mut full: Vec<u8> = Vec::new();
        let mut stream = upstream_resp.bytes_stream();
        let mut read_err: Option<String> = None;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if full.len() + bytes.len() > MAX_RESPONSE_BODY {
                        read_err = Some("上游响应过大".to_string());
                        break;
                    }
                    full.extend_from_slice(&bytes);
                }
                Err(e) => {
                    read_err = Some(format!("上游响应读取失败: {e}"));
                    break;
                }
            }
        }
        if let Some(err) = read_err {
            store_message(
                &shared,
                agg_id,
                Some(plan_id),
                Some(binding_id),
                &method,
                &path,
                502,
                &req_body_stored,
                &err,
                usage::extract_model(&req_body_stored, ""),
                &usage::Usage::default(),
                started,
            )
            .await;
            return error_response(StatusCode::BAD_GATEWAY, "api_error", &err);
        }

        let text = String::from_utf8_lossy(&full).to_string();
        let u = usage::parse_usage(&content_type, &text);
        let resp_stored = cap_store(&text);
        let model = usage::extract_model(&req_body_stored, &text);
        store_message(
            &shared,
            agg_id,
            Some(plan_id),
            Some(binding_id),
            &method,
            &path,
            status_code,
            &req_body_stored,
            &resp_stored,
            model,
            &u,
            started,
        )
        .await;
        resp_builder.body(Body::from(full)).unwrap_or_else(|_| Response::new(Body::empty()))
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn lock_db(shared: &ProxyShared) -> MutexGuard<'_, Connection> {
    match shared.db.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 从请求头中提取客户端出示的令牌（Bearer 或 x-api-key）
fn presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer ") {
            return Some(v[7..].trim().to_string());
        }
        return Some(v.trim().to_string());
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
}

/// 截断字符串用于落库（超长标记）
fn cap_store(s: &str) -> String {
    if s.len() <= MAX_STORED_BODY {
        return s.to_string();
    }
    let mut end = MAX_STORED_BODY;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[已截断]", &s[..end])
}

/// SSE 落库文本解码：一次性 lossy 转换（原始字节整体解码，不受分块边界影响）；
/// 截断点落在多字节字符中间时去掉尾部替换符，再补截断标记
fn decode_sse_stored(buf: &[u8], truncated: bool) -> String {
    let mut s = String::from_utf8_lossy(buf).into_owned();
    if truncated {
        if s.ends_with('\u{FFFD}') {
            s.pop();
        }
        s.push_str("\n…[已截断]");
    }
    s
}

/// Anthropic 风格的错误响应体
fn error_response(status: StatusCode, err_type: &str, message: &str) -> Response {
    let body = json!({"error": {"type": err_type, "message": message}}).to_string();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// 写入消息记录 + 累加绑定用量 + 推送前端事件
#[allow(clippy::too_many_arguments)]
async fn store_message(
    shared: &Arc<ProxyShared>,
    aggregator_id: i64,
    plan_id: Option<i64>,
    binding_id: Option<i64>,
    method: &str,
    path: &str,
    status: i64,
    request_body: &str,
    response_body: &str,
    model: String,
    u: &usage::Usage,
    started: Instant,
) {
    let duration_ms = (started.elapsed().as_millis() as i64).max(0);
    let record = {
        let conn = lock_db(shared);
        if let Some(bid) = binding_id {
            if u.total_tokens > 0 {
                let _ = db::add_binding_usage(&conn, bid, u.total_tokens);
            }
        }
        let msg = NewMessage {
            aggregator_id,
            plan_id,
            method: method.to_string(),
            path: path.to_string(),
            status,
            request_body: request_body.to_string(),
            response_body: response_body.to_string(),
            model,
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_creation_tokens: u.cache_creation_tokens,
            total_tokens: u.total_tokens,
            duration_ms,
        };
        match db::insert_message(&conn, &msg).and_then(|id| db::get_message(&conn, id)) {
            Ok(Some(r)) => Some(r),
            _ => {
                eprintln!("[cpm] 消息落库失败");
                None
            }
        }
    };
    if let Some(record) = record {
        if let Some(app) = &shared.app {
            use tauri::Emitter;
            let _ = app.emit("message:new", &record);
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_store_short_untouched() {
        assert_eq!(cap_store("hello"), "hello");
        assert_eq!(cap_store(""), "");
    }

    #[test]
    fn cap_store_truncates_multibyte_on_boundary() {
        // 多字节字符（"汉" 3 字节）铺满超过 2MB，截断点必然落在字符中间
        let s = "汉".repeat(MAX_STORED_BODY / 3 + 10);
        assert!(s.len() > MAX_STORED_BODY);
        let out = cap_store(&s);
        // 截断处回退到字符边界，不 panic、不产生非法切片
        assert!(out.len() < s.len());
        assert!(out.ends_with("\n…[已截断]"), "应带截断标记");
        let body = out.strip_suffix("\n…[已截断]").unwrap();
        assert!(
            body.chars().all(|c| c == '汉'),
            "截断不应撕裂多字节字符"
        );
    }

    #[test]
    fn decode_sse_stored_multibyte_intact() {
        // 分块累积后的完整字节一次性解码，多字节字符不被撕裂
        assert_eq!(decode_sse_stored("abc汉def".as_bytes(), false), "abc汉def");
        assert_eq!(decode_sse_stored(b"", false), "");
    }

    #[test]
    fn decode_sse_stored_truncated_mid_char() {
        // 截断点落在 "汉"（E6 B1 89）中间：撕裂出的替换符去掉，再补标记
        let out = decode_sse_stored(b"abc\xe6\xb1", true);
        assert_eq!(out, "abc\n…[已截断]");
        assert!(!out.contains('\u{FFFD}'), "不应残留替换符: {out:?}");
    }

    #[test]
    fn decode_sse_stored_truncated_on_boundary() {
        // 截断点恰好是字符边界：原样保留再补标记
        assert_eq!(decode_sse_stored("abc汉".as_bytes(), true), "abc汉\n…[已截断]");
    }
}
