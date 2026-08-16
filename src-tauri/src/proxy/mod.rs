pub mod handler;
pub mod strategy;
pub mod usage;

use crate::db;
use crate::state::RunningServer;
use axum::routing::any;
use axum::Router;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub use handler::ProxyShared;

/// 组装反向代理路由（任意 method/path 兜底）
pub fn build_router(shared: Arc<ProxyShared>) -> Router {
    Router::new().fallback(any(handler::proxy)).with_state(shared)
}

/// 启动一个聚合器的对外服务：绑定端口 + spawn Axum（可优雅关停）。
/// app 为 None 时不向前端推送事件（用于测试）。
pub async fn start_server(
    app: Option<tauri::AppHandle>,
    db_conn: Arc<Mutex<Connection>>,
    aggregator: db::Aggregator,
) -> Result<RunningServer, String> {
    let addr = format!("127.0.0.1:{}", aggregator.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("端口 {} 绑定失败：{e}", aggregator.port))?;

    let cancel = CancellationToken::new();
    let shutdown_token = cancel.clone();

    let shared = Arc::new(ProxyShared {
        aggregator_id: aggregator.id,
        db: db_conn,
        client: build_client(),
        app,
    });
    let router = build_router(shared);

    let port = aggregator.port as u16;
    let task = tauri::async_runtime::spawn(async move {
        let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
            shutdown_token.cancelled().await;
        });
        if let Err(e) = serve.await {
            eprintln!("[cpm] 聚合器服务异常退出: {e}");
        }
    });

    Ok(RunningServer { cancel, port, task })
}

/// 无总超时（SSE 可能持续数分钟），仅设连接超时
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("reqwest client 构建失败")
}
