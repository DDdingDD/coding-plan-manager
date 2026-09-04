use crate::db::{self, MessageRow, MessageSummary, StatsBucket, TripStats, UsageStats};
use crate::state::AppState;
use rusqlite::Connection;
use serde::Serialize;
use std::sync::{Arc, Mutex};

/// list_messages 返回：列表行不含请求/响应体（详情用 get_message 拉全量）
#[derive(Serialize)]
pub struct MessagePage {
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
    pub items: Vec<MessageSummary>,
}

fn e2s(e: rusqlite::Error) -> String {
    format!("数据库错误: {e}")
}

/// 在阻塞线程池执行数据库操作。命令必须是 async fn 才能离开主线程
/// （Tauri v2 同步命令在主线程执行，慢查询会冻结 UI）；统计查询即使已走
/// 覆盖索引，仍可能与代理的大 body 落库在同一个 Mutex 上互斥，
/// 放入 spawn_blocking 避免锁等待占住 tokio worker 线程
async fn run_db<T, F>(state: &tauri::State<'_, AppState>, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T, rusqlite::Error> + Send + 'static,
{
    let db: Arc<Mutex<Connection>> = state.inner().db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = match db.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&conn)
    })
    .await
    .map_err(|e| format!("后台任务执行失败: {e}"))?
    .map_err(e2s)
}

#[tauri::command]
pub async fn list_messages(
    state: tauri::State<'_, AppState>,
    aggregator_id: Option<i64>,
    page: Option<i64>,
    page_size: Option<i64>,
) -> Result<MessagePage, String> {
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(20).clamp(1, 200);
    let offset = (page - 1) * page_size;
    let (total, items) =
        run_db(&state, move |c| db::list_messages(c, aggregator_id, page_size, offset)).await?;
    Ok(MessagePage { total, page, page_size, items })
}

#[tauri::command]
pub async fn get_message(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<MessageRow, String> {
    run_db(&state, move |c| db::get_message(c, id))
        .await?
        .ok_or_else(|| "消息不存在".into())
}

#[tauri::command]
pub async fn clear_messages(
    state: tauri::State<'_, AppState>,
    aggregator_id: Option<i64>,
) -> Result<usize, String> {
    run_db(&state, move |c| db::clear_messages(c, aggregator_id)).await
}

#[tauri::command]
pub async fn global_stats(state: tauri::State<'_, AppState>) -> Result<UsageStats, String> {
    run_db(&state, db::global_stats).await
}

/// 小计里程：自上次手动重置以来的用量统计（未重置时为全部历史）
#[tauri::command]
pub async fn trip_stats(state: tauri::State<'_, AppState>) -> Result<TripStats, String> {
    run_db(&state, db::trip_stats).await
}

/// 重置小计里程（起点改为当前时间并清除暂停状态，不影响累计统计）
#[tauri::command]
pub async fn reset_trip(state: tauri::State<'_, AppState>) -> Result<TripStats, String> {
    run_db(&state, db::reset_trip).await
}

/// 暂停/继续小计里程（暂停期间的用量不补计，不影响累计统计）
#[tauri::command]
pub async fn toggle_trip_pause(state: tauri::State<'_, AppState>) -> Result<TripStats, String> {
    run_db(&state, db::toggle_trip_pause).await
}

/// 按天统计（近 days 天，缺省 30）
#[tauri::command]
pub async fn daily_stats(
    state: tauri::State<'_, AppState>,
    aggregator_id: Option<i64>,
    days: Option<i64>,
) -> Result<Vec<StatsBucket>, String> {
    let days = days.unwrap_or(30).clamp(1, 365);
    run_db(&state, move |c| db::stats_daily(c, aggregator_id, days)).await
}

/// 按小时统计（date 为 "YYYY-MM-DD"，缺省今天）
#[tauri::command]
pub async fn hourly_stats(
    state: tauri::State<'_, AppState>,
    aggregator_id: Option<i64>,
    date: Option<String>,
) -> Result<Vec<StatsBucket>, String> {
    let date = date.unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    run_db(&state, move |c| db::stats_hourly(c, aggregator_id, &date)).await
}

/// 按模型统计（days 缺省为全部时间）
#[tauri::command]
pub async fn model_stats(
    state: tauri::State<'_, AppState>,
    aggregator_id: Option<i64>,
    days: Option<i64>,
) -> Result<Vec<StatsBucket>, String> {
    let days = days.map(|d| d.clamp(1, 365));
    run_db(&state, move |c| db::stats_by_model(c, aggregator_id, days)).await
}
