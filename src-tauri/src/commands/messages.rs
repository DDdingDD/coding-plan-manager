use crate::db::{self, MessageRow, UsageStats};
use crate::state::AppState;
use serde::Serialize;

#[derive(Serialize)]
pub struct MessagePage {
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
    pub items: Vec<MessageRow>,
}

fn e2s(e: rusqlite::Error) -> String {
    format!("数据库错误: {e}")
}

fn lock(state: &AppState) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
    match state.db.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[tauri::command]
pub fn list_messages(
    state: tauri::State<'_, AppState>,
    aggregator_id: Option<i64>,
    page: Option<i64>,
    page_size: Option<i64>,
) -> Result<MessagePage, String> {
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(20).clamp(1, 200);
    let offset = (page - 1) * page_size;
    let conn = lock(&state);
    let (total, items) =
        db::list_messages(&conn, aggregator_id, page_size, offset).map_err(e2s)?;
    Ok(MessagePage { total, page, page_size, items })
}

#[tauri::command]
pub fn get_message(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<MessageRow, String> {
    let conn = lock(&state);
    db::get_message(&conn, id)
        .map_err(e2s)?
        .ok_or_else(|| "消息不存在".into())
}

#[tauri::command]
pub fn clear_messages(
    state: tauri::State<'_, AppState>,
    aggregator_id: Option<i64>,
) -> Result<usize, String> {
    let conn = lock(&state);
    db::clear_messages(&conn, aggregator_id).map_err(e2s)
}

#[tauri::command]
pub fn global_stats(state: tauri::State<'_, AppState>) -> Result<UsageStats, String> {
    let conn = lock(&state);
    db::global_stats(&conn).map_err(e2s)
}
