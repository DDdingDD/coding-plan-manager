use crate::db::{self, CodingPlan, UsageStats};
use crate::state::AppState;
use serde::Serialize;

#[derive(Serialize)]
pub struct PlanView {
    #[serde(flatten)]
    pub plan: CodingPlan,
    pub stats: UsageStats,
}

fn e2s(e: rusqlite::Error) -> String {
    format!("数据库错误: {e}")
}

#[tauri::command]
pub fn list_plans(state: tauri::State<'_, AppState>) -> Result<Vec<PlanView>, String> {
    let conn = lock(&state);
    let plans = db::list_plans(&conn).map_err(e2s)?;
    let views = plans
        .into_iter()
        .map(|p| {
            let stats = db::plan_stats(&conn, p.id).unwrap_or_default();
            PlanView { plan: p, stats }
        })
        .collect();
    Ok(views)
}

#[tauri::command]
pub fn create_plan(
    state: tauri::State<'_, AppState>,
    name: String,
    base_url: String,
    auth_token: String,
    remark: Option<String>,
) -> Result<CodingPlan, String> {
    let name = name.trim().to_string();
    let base_url = base_url.trim().to_string();
    let auth_token = auth_token.trim().to_string();
    if name.is_empty() {
        return Err("计划名称不能为空".into());
    }
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err("BASE_URL 必须以 http:// 或 https:// 开头".into());
    }
    if auth_token.is_empty() {
        return Err("AUTH_TOKEN 不能为空".into());
    }
    let conn = lock(&state);
    db::create_plan(&conn, &name, &base_url, &auth_token, remark.as_deref().unwrap_or(""))
        .map_err(e2s)
}

#[tauri::command]
pub fn update_plan(
    state: tauri::State<'_, AppState>,
    id: i64,
    name: String,
    base_url: String,
    auth_token: String,
    remark: Option<String>,
    enabled: Option<bool>,
) -> Result<CodingPlan, String> {
    let name = name.trim().to_string();
    let base_url = base_url.trim().to_string();
    let auth_token = auth_token.trim().to_string();
    if name.is_empty() {
        return Err("计划名称不能为空".into());
    }
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err("BASE_URL 必须以 http:// 或 https:// 开头".into());
    }
    let conn = lock(&state);
    let existing = db::get_plan(&conn, id).map_err(e2s)?.ok_or("计划不存在")?;
    let enabled = enabled.unwrap_or(existing.enabled);
    db::update_plan(
        &conn,
        id,
        &name,
        &base_url,
        &auth_token,
        remark.as_deref().unwrap_or(""),
        enabled,
    )
    .map_err(e2s)?
    .ok_or_else(|| "计划不存在".into())
}

#[tauri::command]
pub fn delete_plan(state: tauri::State<'_, AppState>, id: i64) -> Result<(), String> {
    let conn = lock(&state);
    let n = db::delete_plan(&conn, id).map_err(e2s)?;
    if n == 0 {
        return Err("计划不存在".into());
    }
    Ok(())
}

fn lock(state: &AppState) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
    match state.db.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}
