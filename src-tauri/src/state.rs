use crate::body::BodyStore;
use crate::maintenance::Maintenance;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;
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
    pub db_path: PathBuf,
    pub servers: Arc<Mutex<HashMap<i64, RunningServer>>>,
    /// 请求体前缀增量缓存：代理落库、清空消息、删除聚合器共用同一份
    /// （锁序恒为 DB → bodies）。历史回填用自己的一份本地缓存，不进这里
    pub bodies: Arc<BodyStore>,
    /// 库维护任务（压缩历史数据 / 整理文件）：同一时刻最多一个
    pub maintenance: Arc<Maintenance>,
}
