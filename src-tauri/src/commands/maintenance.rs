//! 库维护命令：压缩历史数据 / 整理文件 / 取消。thin wrapper，任务逻辑在 maintenance 模块

use crate::maintenance::{self, Maintenance};
use crate::state::AppState;
use std::sync::Arc;

/// 启动「压缩历史数据」：有聚合器在转发时自动改小批量并在批间歇息，
/// 代理请求最多多等一批；可在配置页取消、续跑
#[tauri::command]
pub fn start_compress_messages(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let servers = Arc::clone(&state.servers);
    let proxy_busy: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
        !servers.lock().unwrap_or_else(|p| p.into_inner()).is_empty()
    });
    state.maintenance.start_compress(
        Arc::clone(&state.db),
        proxy_busy,
        maintenance::BACKFILL_LEVEL,
    )
}

/// 启动「整理文件」：整理全程独占数据库连接，运行中的聚合器请求会挂起数分钟，
/// 因此要求先停止全部聚合器（前端按钮同时置灰）
#[tauri::command]
pub fn start_vacuum(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let running = state.servers.lock().unwrap_or_else(|p| p.into_inner()).len();
    if running > 0 {
        return Err(format!("请先停止所有运行中的聚合器（当前 {running} 个在运行）"));
    }
    state.maintenance.start_vacuum(Arc::clone(&state.db))
}

/// 请求取消当前维护任务（压缩在当前批结束后停，整理为 SQLite 原子中断）。返回是否确有任务在运行
#[tauri::command]
pub fn cancel_maintenance(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.maintenance.request_cancel())
}

/// 供其它命令判断是否允许启动聚合器
pub fn vacuum_blocker(m: &Maintenance) -> Option<String> {
    m.is_vacuuming().then(|| "正在整理数据库文件，完成或取消后才能启动聚合器".to_string())
}
