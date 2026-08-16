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
}

/// 启动 mock 上游，返回 (base_url, 收到的鉴权头记录)
async fn spawn_mock_upstream(mode: MockMode) -> (String, Arc<Mutex<Vec<String>>>) {
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let app = {
        let captured = captured.clone();
        axum::Router::new().fallback(move |req: Request| {
            let captured = captured.clone();
            async move {
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

                let (ct, body) = match mode {
                    MockMode::Json => (
                        "application/json",
                        r#"{"id":"m1","usage":{"input_tokens":50,"output_tokens":10}}"#.to_string(),
                    ),
                    MockMode::Sse => (
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
                };
                Response::builder()
                    .status(200)
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
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages?beta=true")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(r#"{"model":"claude-x","stream":false}"#))
        .unwrap();
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
