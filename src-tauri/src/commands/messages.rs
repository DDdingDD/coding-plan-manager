use crate::db::{self, MessageRow, StatsBucket, UsageStats};
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

/// 按天统计（近 days 天，缺省 30）
#[tauri::command]
pub fn daily_stats(
    state: tauri::State<'_, AppState>,
    aggregator_id: Option<i64>,
    days: Option<i64>,
) -> Result<Vec<StatsBucket>, String> {
    let days = days.unwrap_or(30).clamp(1, 365);
    let conn = lock(&state);
    db::stats_daily(&conn, aggregator_id, days).map_err(e2s)
}

/// 按小时统计（date 为 "YYYY-MM-DD"，缺省今天）
#[tauri::command]
pub fn hourly_stats(
    state: tauri::State<'_, AppState>,
    aggregator_id: Option<i64>,
    date: Option<String>,
) -> Result<Vec<StatsBucket>, String> {
    let date = date.unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    let conn = lock(&state);
    db::stats_hourly(&conn, aggregator_id, &date).map_err(e2s)
}

/// 按模型统计
#[tauri::command]
pub fn model_stats(
    state: tauri::State<'_, AppState>,
    aggregator_id: Option<i64>,
) -> Result<Vec<StatsBucket>, String> {
    let conn = lock(&state);
    db::stats_by_model(&conn, aggregator_id).map_err(e2s)
}
