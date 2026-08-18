//! 端到端集成测试：直接驱动反向代理 router + 本地 mock 上游，
//! 覆盖鉴权、令牌替换、阈值轮转、清零回绕、用量统计、消息落库、SSE 流式。

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use coding_plan_manager_lib::db;
use coding_plan_manager_lib::proxy::{self, ProxyShared};
use http_body_util::BodyExt;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

type Conn = Arc<Mutex<rusqlite::Connection>>;

// ---------------------------------------------------------------------------
// mock 上游
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq)]
enum MockMode {
    /// Anthropic JSON：input 50 / output 10
    Json,
    /// Anthropic SSE：input 30 / output 最终 9
    Sse,
    /// Anthropic JSON + prompt caching：input 50 / cache_read 900 / cache_creation 40 / output 10
    CacheJson,
    /// 固定错误：429 + Anthropic 风格错误体
    Error,
    /// 回显：JSON 返回收到的 method/uri/headers，用于断言头透传与方法保真
    Echo,
}

/// 启动 mock 上游，返回 (base_url, 收到的鉴权头记录)
async fn spawn_mock_upstream(mode: MockMode) -> (String, Arc<Mutex<Vec<String>>>) {
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let app = {
        let captured = captured.clone();
        axum::Router::new().fallback(move |req: Request| {
            let captured = captured.clone();
            async move {
                let method = req.method().as_str().to_string();
                let uri = req
                    .uri()
                    .path_and_query()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let header_pairs: Vec<(String, String)> = req
                    .headers()
                    .iter()
                    .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                let auth = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let api_key = req
                    .headers()
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                captured.lock().unwrap().push(format!("{auth}|{api_key}"));
                let _ = axum::body::to_bytes(req.into_body(), 1024 * 1024).await;

                let (status, ct, body) = match mode {
                    MockMode::Json => (
                        200,
                        "application/json",
                        r#"{"id":"m1","usage":{"input_tokens":50,"output_tokens":10}}"#.to_string(),
                    ),
                    MockMode::Sse => (
                        200,
                        "text/event-stream",
                        concat!(
                            "event: message_start\n",
                            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":30,\"output_tokens\":1}}}\n\n",
                            "event: content_block_delta\n",
                            "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n",
                            "event: message_delta\n",
                            "data: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":9}}\n\n",
                        )
                        .to_string(),
                    ),
                    MockMode::CacheJson => (
                        200,
                        "application/json",
                        r#"{"id":"m1","usage":{"input_tokens":50,"cache_read_input_tokens":900,"cache_creation_input_tokens":40,"output_tokens":10}}"#.to_string(),
                    ),
                    MockMode::Error => (
                        429,
                        "application/json",
                        r#"{"type":"error","error":{"type":"rate_limit_error","message":"超出速率限制"}}"#.to_string(),
                    ),
                    MockMode::Echo => {
                        // 重复头的多个值合并为 "a, b"，便于断言 append 语义
                        let mut headers = serde_json::Map::new();
                        for (n, v) in &header_pairs {
                            match headers.get_mut(n) {
                                Some(serde_json::Value::String(prev)) => {
                                    *prev = format!("{prev}, {v}");
                                }
                                _ => {
                                    headers.insert(
                                        n.clone(),
                                        serde_json::Value::String(v.clone()),
                                    );
                                }
                            }
                        }
                        (
                            200,
                            "application/json",
                            serde_json::json!({"method": method, "uri": uri, "headers": headers})
                                .to_string(),
                        )
                    }
                };
                Response::builder()
                    .status(status)
                    .header("content-type", ct)
                    .body(Body::from(body))
                    .unwrap()
            }
        })
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{}", addr.port()), captured)
}

// ---------------------------------------------------------------------------
// 测试辅助
// ---------------------------------------------------------------------------

fn lock(db: &Conn) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
    db.lock().unwrap()
}

async fn send(router: &axum::Router, token: &str) -> (u16, String) {
    send_raw(
        router,
        "POST",
        "/v1/messages?beta=true",
        &[("authorization", format!("Bearer {token}"))],
        r#"{"model":"claude-x","stream":false}"#,
    )
    .await
}

/// 以指定 method/uri/头/体构造请求并 oneshot 驱动代理
async fn send_raw(
    router: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, String)],
    body: &str,
) -> (u16, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, value.as_str());
    }
    let req = builder.body(Body::from(body.to_string())).unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// 轮询等待消息落库（SSE 分支的落库在后台任务中完成）
async fn wait_for_messages(conn: &Conn, agg_id: i64, min: i64) {
    for _ in 0..200 {
        let total = {
            let c = lock(conn);
            db::list_messages(&c, Some(agg_id), 1, 0).unwrap().0
        };
        if total >= min {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("消息未在预期时间内落库");
}

/// 初始化内存库 + 聚合器（按 plan_tokens 顺序绑定各计划），返回 (连接, 路由, 聚合器 id)
async fn setup_router(
    upstream: &str,
    plan_tokens: &[&str],
    port: i64,
    agg_token: &str,
    threshold: i64,
) -> (Conn, axum::Router, i64) {
    let conn: Conn = Arc::new(Mutex::new(db::init_db(":memory:").unwrap()));
    let agg_id = {
        let c = lock(&conn);
        let plan_ids: Vec<i64> = plan_tokens
            .iter()
            .enumerate()
            .map(|(i, t)| db::create_plan(&c, &format!("P{i}"), upstream, t, "").unwrap().id)
            .collect();
        let agg = db::create_aggregator(&c, "g", port, agg_token, threshold).unwrap();
        db::set_aggregator_plans(&c, agg.id, &plan_ids).unwrap();
        agg.id
    };
    let shared = Arc::new(ProxyShared {
        aggregator_id: agg_id,
        db: conn.clone(),
        client: reqwest::Client::new(),
        app: None,
    });
    (conn, proxy::build_router(shared), agg_id)
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

/// 主链路：鉴权失败 401 -> 正常转发（令牌替换）-> 阈值轮转 -> 全部超额清零回绕 -> 统计与消息
#[tokio::test(flavor = "multi_thread")]
async fn e2e_auth_rotation_stats_and_wraparound() {
    let (upstream, captured) = spawn_mock_upstream(MockMode::Json).await;

    let conn: Conn = Arc::new(Mutex::new(db::init_db(":memory:").unwrap()));
    let (pa, pb, agg) = {
        let c = lock(&conn);
        let pa = db::create_plan(&c, "A", &upstream, "tokA", "").unwrap();
        let pb = db::create_plan(&c, "B", &upstream, "tokB", "").unwrap();
        // 阈值 100，每响应 total=60
        let agg = db::create_aggregator(&c, "g", 8300, "cpm-test", 100).unwrap();
        db::set_aggregator_plans(&c, agg.id, &[pa.id, pb.id]).unwrap();
        (pa, pb, agg)
    };

    let shared = Arc::new(ProxyShared {
        aggregator_id: agg.id,
        db: conn.clone(),
        client: reqwest::Client::new(),
        app: None,
    });
    let router = proxy::build_router(shared);

    // 1) 错误令牌 -> 401，且记录一条未授权消息
    let (status, body) = send(&router, "wrong-token").await;
    assert_eq!(status, 401);
    assert!(body.contains("authentication_error"));

    // 2) 5 次正常请求：轮转序列 A A B B（超额清零后回绕）A
    for _ in 0..5 {
        let (status, body) = send(&router, "cpm-test").await;
        assert_eq!(status, 200, "转发应成功: {body}");
        assert!(body.contains("\"input_tokens\":50"));
    }

    // 3) 上游收到的是各 plan 的令牌（Bearer + x-api-key 双头）
    {
        let cap = captured.lock().unwrap();
        assert_eq!(cap.len(), 5);
        let seq: Vec<&str> = cap.iter().map(|s| s.split('|').next().unwrap()).collect();
        assert_eq!(
            seq,
            vec!["Bearer tokA", "Bearer tokA", "Bearer tokB", "Bearer tokB", "Bearer tokA"],
            "阈值轮转序列应为 A A B B A"
        );
        assert!(cap[0].ends_with("|tokA"), "应同时注入 x-api-key: {}", cap[0]);
    }

    // 4) 统计：A 承接 r1/r2/r5=180，B 承接 r3/r4=120；绑定用量 A 清零回绕后=60，B=120
    {
        let c = lock(&conn);
        let sa = db::plan_stats(&c, pa.id).unwrap();
        let sb = db::plan_stats(&c, pb.id).unwrap();
        assert_eq!(sa.total_tokens, 180);
        assert_eq!(sb.total_tokens, 120);

        let bindings = db::list_bindings(&c, agg.id).unwrap();
        let a = bindings.iter().find(|b| b.plan.id == pa.id).unwrap();
        let b = bindings.iter().find(|b| b.plan.id == pb.id).unwrap();
        assert_eq!(a.used_tokens, 60, "A 清零回绕后又累计 60");
        assert_eq!(b.used_tokens, 0, "r5 全部超额清零时 B 的绑定用量一并清零");

        // 消息：1 条 401 + 5 条 200，均含聚合器/计划名
        let (total, items) = db::list_messages(&c, Some(agg.id), 10, 0).unwrap();
        assert_eq!(total, 6);
        let ok_rows: Vec<_> = items.iter().filter(|m| m.status == 200).collect();
        assert_eq!(ok_rows.len(), 5);
        assert!(ok_rows.iter().all(|m| m.plan_name.as_deref() == Some("A") || m.plan_name.as_deref() == Some("B")));
        assert!(items.iter().any(|m| m.status == 401 && m.plan_id.is_none()));

        let gs = db::global_stats(&c).unwrap();
        assert_eq!(gs.requests, 6);
        assert_eq!(gs.total_tokens, 300);
    }
}

/// SSE 流式：透传 + Anthropic 事件流用量解析
#[tokio::test(flavor = "multi_thread")]
async fn e2e_sse_stream_usage() {
    let (upstream, _captured) = spawn_mock_upstream(MockMode::Sse).await;

    let conn: Conn = Arc::new(Mutex::new(db::init_db(":memory:").unwrap()));
    let agg = {
        let c = lock(&conn);
        let p = db::create_plan(&c, "S", &upstream, "tokS", "").unwrap();
        let agg = db::create_aggregator(&c, "g", 8301, "cpm-sse", 1000).unwrap();
        db::set_aggregator_plans(&c, agg.id, &[p.id]).unwrap();
        agg
    };

    let shared = Arc::new(ProxyShared {
        aggregator_id: agg.id,
        db: conn.clone(),
        client: reqwest::Client::new(),
        app: None,
    });
    let router = proxy::build_router(shared);

    let (status, body) = send(&router, "cpm-sse").await;
    assert_eq!(status, 200);
    assert!(body.contains("event: message_start"), "SSE 应原样透传");
    assert!(body.contains("message_delta"));

    wait_for_messages(&conn, agg.id, 1).await;

    let c = lock(&conn);
    let (_, items) = db::list_messages(&c, Some(agg.id), 5, 0).unwrap();
    let m = items.iter().find(|m| m.status == 200).unwrap();
    assert_eq!(m.prompt_tokens, 30);
    assert_eq!(m.completion_tokens, 9, "message_delta 的 output_tokens 应覆盖 message_start 的初始值");
    assert_eq!(m.total_tokens, 39);
    let bindings = db::list_bindings(&c, agg.id).unwrap();
    assert_eq!(bindings[0].used_tokens, 39, "绑定用量应累加 39");
}

/// 上游不可达 -> 502 + 消息记录
#[tokio::test(flavor = "multi_thread")]
async fn e2e_upstream_unreachable_502() {
    let conn: Conn = Arc::new(Mutex::new(db::init_db(":memory:").unwrap()));
    let agg = {
        let c = lock(&conn);
        // 端口 1 几乎必然无服务
        let p = db::create_plan(&c, "dead", "http://127.0.0.1:1", "tokD", "").unwrap();
        let agg = db::create_aggregator(&c, "g", 8302, "cpm-502", 1000).unwrap();
        db::set_aggregator_plans(&c, agg.id, &[p.id]).unwrap();
        agg
    };

    let shared = Arc::new(ProxyShared {
        aggregator_id: agg.id,
        db: conn.clone(),
        client: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap(),
        app: None,
    });
    let router = proxy::build_router(shared);

    let (status, body) = send(&router, "cpm-502").await;
    assert_eq!(status, 502);
    assert!(body.contains("api_error"));

    let c = lock(&conn);
    let (_, items) = db::list_messages(&c, Some(agg.id), 5, 0).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].status, 502);
    assert_eq!(items[0].plan_name.as_deref(), Some("dead"));
}

/// 未绑定任何计划 -> 503
#[tokio::test(flavor = "multi_thread")]
async fn e2e_no_plan_503() {
    let conn: Conn = Arc::new(Mutex::new(db::init_db(":memory:").unwrap()));
    let agg = {
        let c = lock(&conn);
        db::create_aggregator(&c, "g", 8303, "cpm-503", 1000).unwrap()
    };

    let shared = Arc::new(ProxyShared {
        aggregator_id: agg.id,
        db: conn.clone(),
        client: reqwest::Client::new(),
        app: None,
    });
    let router = proxy::build_router(shared);

    let (status, body) = send(&router, "cpm-503").await;
    assert_eq!(status, 503);
    assert!(body.contains("overloaded_error"));
}

/// 真实 TCP 层：start_server 监听 -> HTTP 请求转发到 mock 上游 -> 停止后端口关闭
#[tokio::test(flavor = "multi_thread")]
async fn e2e_tcp_server_start_stop() {
    let (upstream, captured) = spawn_mock_upstream(MockMode::Json).await;

    let conn: Conn = Arc::new(Mutex::new(db::init_db(":memory:").unwrap()));
    let agg = {
        let c = lock(&conn);
        let p = db::create_plan(&c, "T", &upstream, "tokT", "").unwrap();
        let agg = db::create_aggregator(&c, "g", 8305, "cpm-tcp", 1000).unwrap();
        db::set_aggregator_plans(&c, agg.id, &[p.id]).unwrap();
        agg
    };

    // 启动真实服务
    let server = proxy::start_server(None, conn.clone(), agg.clone())
        .await
        .expect("端口绑定与启动应成功");

    // 通过真实 TCP 请求
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", agg.port))
        .header("authorization", "Bearer cpm-tcp")
        .json(&serde_json::json!({"model": "claude-x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["usage"]["input_tokens"], 50);

    // 上游确实收到了 plan 的令牌
    assert_eq!(
        captured.lock().unwrap()[0],
        "Bearer tokT|tokT",
        "真实转发应把鉴权头替换为 plan 的 AUTH_TOKEN"
    );

    // 消息落库
    wait_for_messages(&conn, agg.id, 1).await;

    // 优雅停止后端口应关闭
    server.cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server.task).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let closed = client
        .post(format!("http://127.0.0.1:{}/v1/messages", agg.port))
        .send()
        .await;
    assert!(closed.is_err(), "停止服务后端口应不再接受连接");
}

/// x-api-key 鉴权：正确令牌放行，错误令牌 401（Claude Code 实际使用该头）
#[tokio::test(flavor = "multi_thread")]
async fn e2e_x_api_key_auth() {
    let (upstream, captured) = spawn_mock_upstream(MockMode::Json).await;
    let (_conn, router, _) = setup_router(&upstream, &["tokK"], 8306, "cpm-key", 1000).await;

    // 正确 x-api-key -> 200
    let (status, body) = send_raw(
        &router,
        "POST",
        "/v1/messages",
        &[("x-api-key", "cpm-key".to_string())],
        r#"{"model":"claude-x"}"#,
    )
    .await;
    assert_eq!(status, 200, "x-api-key 鉴权应放行: {body}");

    // 上游收到的是计划的令牌（客户端原始 x-api-key 不透传）
    assert_eq!(captured.lock().unwrap()[0], "Bearer tokK|tokK");

    // 错误 x-api-key -> 401
    let (status, body) = send_raw(
        &router,
        "POST",
        "/v1/messages",
        &[("x-api-key", "wrong".to_string())],
        r#"{"model":"claude-x"}"#,
    )
    .await;
    assert_eq!(status, 401);
    assert!(body.contains("authentication_error"));
}

/// 上游返回非 200（429）：状态码与错误体应原样透传，消息以该状态落库
#[tokio::test(flavor = "multi_thread")]
async fn e2e_upstream_error_status_passthrough() {
    let (upstream, _captured) = spawn_mock_upstream(MockMode::Error).await;
    let (conn, router, agg_id) = setup_router(&upstream, &["tokE"], 8307, "cpm-err", 1000).await;

    let (status, body) = send(&router, "cpm-err").await;
    assert_eq!(status, 429, "上游状态码应透传");
    assert!(body.contains("rate_limit_error"), "上游错误体应透传: {body}");

    // 非流式分支落库在响应返回前完成；列表行不含体，完整消息经 get_message 拉取
    let c = lock(&conn);
    let (total, items) = db::list_messages(&c, Some(agg_id), 5, 0).unwrap();
    assert_eq!(total, 1);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].status, 429);
    let m = db::get_message(&c, items[0].id).unwrap().unwrap();
    assert!(m.response_body.contains("rate_limit_error"));
    assert_eq!(m.total_tokens, 0, "错误响应无 usage，不计 token");
}

/// 业务头透传 + 跳段/鉴权头剥离：anthropic-* 等透传到上游，
/// 原始 authorization/x-api-key/host/accept-encoding 不透传，替换为计划令牌
#[tokio::test(flavor = "multi_thread")]
async fn e2e_header_passthrough_and_strip() {
    let (upstream, _captured) = spawn_mock_upstream(MockMode::Echo).await;
    let (_conn, router, _) = setup_router(&upstream, &["tokH"], 8308, "cpm-hdr", 1000).await;

    let (status, body) = send_raw(
        &router,
        "POST",
        "/v1/messages?beta=true",
        &[
            ("authorization", "Bearer cpm-hdr".to_string()),
            ("x-api-key", "cpm-hdr".to_string()),
            ("anthropic-version", "2023-06-01".to_string()),
            ("anthropic-beta", "context-1m-2025-08-07".to_string()),
            ("x-custom-header", "keep-me".to_string()),
            ("x-multi-header", "one".to_string()),
            ("x-multi-header", "two".to_string()),
            ("accept-encoding", "gzip".to_string()),
            ("host", "evil.example".to_string()),
        ],
        r#"{"model":"claude-x"}"#,
    )
    .await;
    assert_eq!(status, 200, "转发应成功: {body}");

    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["method"], "POST");
    assert_eq!(v["uri"], "/v1/messages?beta=true", "path 与 query 应保真");
    let h = &v["headers"];

    // 业务头透传
    assert_eq!(h["anthropic-version"], "2023-06-01");
    assert_eq!(h["anthropic-beta"], "context-1m-2025-08-07");
    assert_eq!(h["x-custom-header"], "keep-me");
    // 重复头的多个值都应转发（append 语义，而非只剩最后一个）
    assert_eq!(h["x-multi-header"], "one, two");

    // 鉴权头替换为计划的 AUTH_TOKEN（Bearer + x-api-key 双头）
    assert_eq!(h["authorization"], "Bearer tokH");
    assert_eq!(h["x-api-key"], "tokH");

    // 跳段头/编码协商头剥离
    assert!(h.get("accept-encoding").is_none(), "accept-encoding 应被剥离: {h}");
    assert_ne!(h["host"], "evil.example", "客户端原始 host 不应透传");
}

/// GET 等无 body 方法：透明转发，方法与路径保真
#[tokio::test(flavor = "multi_thread")]
async fn e2e_get_no_body() {
    let (upstream, _captured) = spawn_mock_upstream(MockMode::Echo).await;
    let (_conn, router, _) = setup_router(&upstream, &["tokG"], 8309, "cpm-get", 1000).await;

    let (status, body) = send_raw(
        &router,
        "GET",
        "/v1/models?limit=10",
        &[("authorization", "Bearer cpm-get".to_string())],
        "",
    )
    .await;
    assert_eq!(status, 200, "GET 请求应转发成功: {body}");

    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["method"], "GET");
    assert_eq!(v["uri"], "/v1/models?limit=10");
}

/// 配置热更新：运行中的服务修改 token_threshold 即时生效，无需重启
#[tokio::test(flavor = "multi_thread")]
async fn e2e_threshold_hot_reload() {
    let (upstream, captured) = spawn_mock_upstream(MockMode::Json).await;
    let (conn, router, agg_id) = setup_router(&upstream, &["tokA", "tokB"], 8310, "cpm-hot", 100).await;

    // 请求 1：阈值 100，A 从 0 起步 -> 选 A（用后 60）
    let (status, _) = send(&router, "cpm-hot").await;
    assert_eq!(status, 200);
    assert_eq!(captured.lock().unwrap()[0], "Bearer tokA|tokA");

    // 热更新：阈值降到 60，A 已用 60 -> 下一请求应切到 B（服务未重启）
    {
        let c = lock(&conn);
        db::update_aggregator(&c, agg_id, "g", 8310, 60).unwrap();
    }
    let (status, _) = send(&router, "cpm-hot").await;
    assert_eq!(status, 200);
    assert_eq!(captured.lock().unwrap()[1], "Bearer tokB|tokB", "阈值修改应即时生效");
}

/// prompt caching：cache_read/cache_creation 解析入库并计入 total（原始 token 口径）与绑定用量
#[tokio::test(flavor = "multi_thread")]
async fn e2e_cache_tokens_counted() {
    let (upstream, _captured) = spawn_mock_upstream(MockMode::CacheJson).await;
    let (conn, router, agg_id) = setup_router(&upstream, &["tokC"], 8311, "cpm-cache", 1000).await;

    let (status, body) = send(&router, "cpm-cache").await;
    assert_eq!(status, 200, "转发应成功: {body}");

    // 非流式分支落库在响应返回前完成
    let c = lock(&conn);
    let (_, items) = db::list_messages(&c, Some(agg_id), 5, 0).unwrap();
    let m = &items[0];
    assert_eq!(m.prompt_tokens, 50, "input_tokens 不含缓存部分");
    assert_eq!(m.cache_read_tokens, 900);
    assert_eq!(m.cache_creation_tokens, 40);
    assert_eq!(m.total_tokens, 50 + 900 + 40 + 10, "total 应按原始口径合计四项");

    // 绑定用量（轮转计数）同样按含缓存的 total 累加
    let bindings = db::list_bindings(&c, agg_id).unwrap();
    assert_eq!(bindings[0].used_tokens, 1000, "绑定用量应含缓存 token");

    // 统计聚合带出缓存字段
    let s = db::aggregator_stats(&c, agg_id).unwrap();
    assert_eq!(s.cache_read_tokens, 900);
    assert_eq!(s.cache_creation_tokens, 40);
    assert_eq!(s.total_tokens, 1000);
}
