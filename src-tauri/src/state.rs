use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// 运行中的一个聚合器服务句柄
pub struct RunningServer {
    pub cancel: CancellationToken,
    pub port: u16,
    pub task: tauri::async_runtime::JoinHandle<()>,
}

/// 全局应用状态（由 Tauri 管理）
pub struct AppState {
    pub db: Arc<Mutex<Connection>>,
    pub servers: Arc<Mutex<HashMap<i64, RunningServer>>>,
}
