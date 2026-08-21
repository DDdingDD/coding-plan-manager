use crate::db::{self, Aggregator, UsageStats};
use crate::proxy;
use crate::state::{AppState, RunningServer};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Serialize, Clone)]
pub struct BindingView {
    pub plan_id: i64,
    pub plan_name: String,
    pub enabled: bool,
    pub position: i64,
    pub used_tokens: i64,
    pub token_threshold: i64,
    /// 该计划配置的支持模型（模型匹配策略的路由依据）
    pub models: Vec<String>,
}

#[derive(Serialize, Clone)]
pub struct AggregatorView {
    #[serde(flatten)]
    pub aggregator: Aggregator,
    pub running: bool,
    pub base_url: String,
    pub bindings: Vec<BindingView>,
    pub stats: UsageStats,
    /// 当前转发的计划（下一个请求将使用），由 strategy::peek_plan 无副作用推导
    pub current_plan_id: Option<i64>,
    /// 手动指定的当前计划（DB 原值）：阈值轮转的固定计划（null=自动轮转）；
    /// 模型匹配的粘性目标（与派生值可能短暂不一致，仅前端区分展示用）
    pub manual_current_plan_id: Option<i64>,
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

fn build_view(conn: &rusqlite::Connection, agg: Aggregator, running: bool) -> AggregatorView {
    let bindings = db::list_bindings(conn, agg.id)
        .unwrap_or_default()
        .into_iter()
        .map(|b| BindingView {
            plan_id: b.plan.id,
            plan_name: b.plan.name.clone(),
            enabled: b.plan.enabled,
            position: b.position,
            used_tokens: b.used_tokens,
            token_threshold: agg.token_threshold,
            models: b.plan.models.clone(),
        })
        .collect();
    let stats = db::aggregator_stats(conn, agg.id).unwrap_or_default();
    // 列表/详情不应因策略推导失败而报错，容错为 None（前端显示「无可用计划」）
    let current_plan_id = crate::proxy::strategy::peek_plan(conn, &agg).unwrap_or(None);
    let manual_current_plan_id = agg.current_plan_id;
    AggregatorView {
        base_url: format!("http://127.0.0.1:{}", agg.port),
        running,
        aggregator: agg,
        bindings,
        stats,
        current_plan_id,
        manual_current_plan_id,
    }
}

fn get_view(state: &AppState, id: i64) -> Result<AggregatorView, String> {
    let conn = lock(state);
    let agg = db::get_aggregator(&conn, id)
        .map_err(e2s)?
        .ok_or("聚合器不存在")?;
    let running = state.servers.lock().unwrap().contains_key(&id);
    Ok(build_view(&conn, agg, running))
}

// ---------------------------------------------------------------------------
// 查询
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_aggregators(state: tauri::State<'_, AppState>) -> Result<Vec<AggregatorView>, String> {
    let conn = lock(&state);
    let running_ids: Vec<i64> = state.servers.lock().unwrap().keys().copied().collect();
    let aggs = db::list_aggregators(&conn).map_err(e2s)?;
    Ok(aggs
        .into_iter()
        .map(|a| {
            let id = a.id;
            build_view(&conn, a, running_ids.contains(&id))
        })
        .collect())
}

// ---------------------------------------------------------------------------
// 增删改
// ---------------------------------------------------------------------------

/// 解析创建端口：显式端口校验范围与唯一性；自动分配则在 8300..=8399 中
/// 跳过已占用端口后再做 bind 测试（taken 来自数据库——服务未运行时 bind 测不出占用）
fn resolve_create_port(explicit: Option<i64>, taken: &HashSet<i64>) -> Result<i64, String> {
    match explicit {
        Some(p) if (1..=65535).contains(&p) => {
            if taken.contains(&p) {
                Err(format!("端口 {p} 已被其他聚合器使用"))
            } else {
                Ok(p)
            }
        }
        Some(_) => Err("端口必须在 1-65535 之间".into()),
        None => find_free_port(taken).ok_or_else(|| "8300-8399 范围内没有可用端口".into()),
    }
}

/// 在 8300..=8399 中寻找可用端口（跳过已占用端口）
fn find_free_port(taken: &HashSet<i64>) -> Option<i64> {
    (8300i64..=8399).find(|&p| {
        !taken.contains(&p) && std::net::TcpListener::bind(("127.0.0.1", p as u16)).is_ok()
    })
}

#[tauri::command]
pub fn create_aggregator(
    state: tauri::State<'_, AppState>,
    name: String,
    port: Option<i64>,
    token_threshold: Option<i64>,
    strategy: Option<String>,
) -> Result<AggregatorView, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("聚合器名称不能为空".into());
    }
    let token_threshold = token_threshold.unwrap_or(1_000_000).max(1);
    let strategy = resolve_strategy(strategy.as_deref())?;

    let conn = lock(&state);
    let taken: HashSet<i64> = db::list_aggregators(&conn)
        .map_err(e2s)?
        .iter()
        .map(|a| a.port)
        .collect();
    let port = resolve_create_port(port, &taken)?;
    let token = crate::token::generate_token();
    let agg = db::create_aggregator(&conn, &name, port, &token, token_threshold, &strategy)
        .map_err(e2s)?;
    Ok(build_view(&conn, agg, false))
}

/// 解析策略参数：缺省/空串为阈值轮转；未知值报错（避免静默落库一个不生效的策略）
fn resolve_strategy(strategy: Option<&str>) -> Result<String, String> {
    let s = strategy.unwrap_or("").trim();
    if s.is_empty() {
        return Ok(db::STRATEGY_THRESHOLD_ROTATION.to_string());
    }
    if !db::is_known_strategy(s) {
        return Err(format!("未知的路由策略：{s}"));
    }
    Ok(s.to_string())
}

#[tauri::command]
pub fn update_aggregator(
    state: tauri::State<'_, AppState>,
    id: i64,
    name: String,
    port: i64,
    token_threshold: i64,
    strategy: Option<String>,
) -> Result<AggregatorView, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("聚合器名称不能为空".into());
    }
    if !(1..=65535).contains(&port) {
        return Err("端口必须在 1-65535 之间".into());
    }
    if token_threshold < 1 {
        return Err("token 阈值必须大于 0".into());
    }
    let strategy = resolve_strategy(strategy.as_deref())?;

    let conn = lock(&state);
    let old = db::get_aggregator(&conn, id)
        .map_err(e2s)?
        .ok_or("聚合器不存在")?;
    let running = state.servers.lock().unwrap().contains_key(&id);
    if running && old.port != port {
        return Err("服务运行中不能修改端口，请先停止服务".into());
    }
    let dup = db::list_aggregators(&conn)
        .map_err(e2s)?
        .iter()
        .any(|a| a.id != id && a.port == port);
    if dup {
        return Err(format!("端口 {port} 已被其他聚合器使用"));
    }
    // 策略切换时清除「当前计划」：两种策略对该字段语义不同
    // （模型匹配=粘性目标，阈值轮转=手动固定），沿用旧值会让轮转粘在旧目标上
    if old.strategy != strategy {
        db::clear_current_plan(&conn, id).map_err(e2s)?;
    }
    // 策略热生效：handler 每请求重读配置，无需重启服务
    db::update_aggregator(&conn, id, &name, port, token_threshold, &strategy).map_err(e2s)?;
    drop(conn);
    get_view(&state, id)
}

/// 手动设置「当前计划」（两种策略均可用，须为该聚合器绑定且启用的计划）：
/// - 模型匹配：粘性路由目标（无匹配时的兜底）
/// - 阈值轮转：固定当前转发计划，直至其达阈值/失效后由策略自动清除、恢复轮转
/// plan_id 为 None 时清除指定（阈值轮转恢复自动；模型匹配回退首个启用绑定并固化）
#[tauri::command]
pub fn set_current_plan(
    state: tauri::State<'_, AppState>,
    aggregator_id: i64,
    plan_id: Option<i64>,
) -> Result<AggregatorView, String> {
    let conn = lock(&state);
    db::get_aggregator(&conn, aggregator_id)
        .map_err(e2s)?
        .ok_or("聚合器不存在")?;
    let updated = match plan_id {
        Some(pid) => {
            let binding = db::list_bindings(&conn, aggregator_id)
                .map_err(e2s)?
                .into_iter()
                .find(|b| b.plan.id == pid)
                .ok_or_else(|| format!("计划 {pid} 未绑定到该聚合器"))?;
            if !binding.plan.enabled {
                return Err("该计划已禁用，不能设为当前计划".into());
            }
            db::set_current_plan(&conn, aggregator_id, pid).map_err(e2s)?
        }
        None => db::clear_current_plan(&conn, aggregator_id).map_err(e2s)?,
    };
    if !updated {
        return Err("聚合器不存在".into());
    }
    drop(conn);
    get_view(&state, aggregator_id)
}

#[tauri::command]
pub async fn delete_aggregator(state: tauri::State<'_, AppState>, id: i64) -> Result<(), String> {
    // 先停止运行中的服务
    let handle = state.servers.lock().unwrap().remove(&id);
    if let Some(h) = handle {
        h.cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h.task).await;
    }
    let conn = lock(&state);
    let n = db::delete_aggregator(&conn, id).map_err(e2s)?;
    if n == 0 {
        return Err("聚合器不存在".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 绑定计划
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn set_aggregator_plans(
    state: tauri::State<'_, AppState>,
    aggregator_id: i64,
    plan_ids: Vec<i64>,
) -> Result<AggregatorView, String> {
    let conn = lock(&state);
    db::get_aggregator(&conn, aggregator_id)
        .map_err(e2s)?
        .ok_or("聚合器不存在")?;
    for pid in &plan_ids {
        if db::get_plan(&conn, *pid).map_err(e2s)?.is_none() {
            return Err(format!("计划 {pid} 不存在"));
        }
    }
    let plan_ids = dedup_keep_order(plan_ids);
    db::set_aggregator_plans(&conn, aggregator_id, &plan_ids).map_err(e2s)?;
    let running = state.servers.lock().unwrap().contains_key(&aggregator_id);
    let agg = db::get_aggregator(&conn, aggregator_id)
        .map_err(e2s)?
        .ok_or("聚合器不存在")?;
    Ok(build_view(&conn, agg, running))
}

fn dedup_keep_order(ids: Vec<i64>) -> Vec<i64> {
    let mut seen = std::collections::HashSet::new();
    ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

#[tauri::command]
pub fn reset_aggregator_usage(
    state: tauri::State<'_, AppState>,
    aggregator_id: i64,
) -> Result<AggregatorView, String> {
    let conn = lock(&state);
    db::reset_binding_usage(&conn, aggregator_id).map_err(e2s)?;
    let running = state.servers.lock().unwrap().contains_key(&aggregator_id);
    let agg = db::get_aggregator(&conn, aggregator_id)
        .map_err(e2s)?
        .ok_or("聚合器不存在")?;
    Ok(build_view(&conn, agg, running))
}

// ---------------------------------------------------------------------------
// 启停
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn start_aggregator(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<AggregatorView, String> {
    {
        let servers = state.servers.lock().unwrap();
        if servers.contains_key(&id) {
            drop(servers);
            return get_view(&state, id);
        }
    }
    let agg = {
        let conn = lock(&state);
        db::get_aggregator(&conn, id)
            .map_err(e2s)?
            .ok_or("聚合器不存在")?
    };
    let db_clone: Arc<Mutex<rusqlite::Connection>> = state.db.clone();
    let running = proxy::start_server(Some(app), db_clone, agg).await?;
    state.servers.lock().unwrap().insert(
        id,
        RunningServer {
            cancel: running.cancel,
            port: running.port,
            task: running.task,
        },
    );
    get_view(&state, id)
}

#[tauri::command]
pub async fn stop_aggregator(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<AggregatorView, String> {
    let handle = state.servers.lock().unwrap().remove(&id);
    if let Some(h) = handle {
        h.cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h.task).await;
    }
    get_view(&state, id)
}

/// 供前端异常兜底：当前运行中的聚合器 id -> 端口
#[allow(dead_code)]
pub fn running_snapshot(state: &AppState) -> HashMap<i64, u16> {
    state
        .servers
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (*k, v.port))
        .collect()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn taken(ports: &[i64]) -> HashSet<i64> {
        ports.iter().copied().collect()
    }

    #[test]
    fn explicit_port_range_and_uniqueness() {
        let t = taken(&[8300, 8301]);
        assert_eq!(resolve_create_port(Some(9000), &t), Ok(9000));
        let err = resolve_create_port(Some(8300), &t).unwrap_err();
        assert!(err.contains("已被其他聚合器使用"), "冲突端口应报错: {err}");
        let err = resolve_create_port(Some(0), &t).unwrap_err();
        assert!(err.contains("1-65535"), "越界端口应报错: {err}");
        let err = resolve_create_port(Some(70000), &t).unwrap_err();
        assert!(err.contains("1-65535"), "越界端口应报错: {err}");
    }

    #[test]
    fn auto_assign_skips_taken_without_binding() {
        // 8300-8399 全部占用：直接失败，不触发任何 bind（taken 短路保证确定性）
        let t: HashSet<i64> = (8300..=8399).collect();
        let err = resolve_create_port(None, &t).unwrap_err();
        assert!(err.contains("没有可用端口"), "全部占用应报错: {err}");
    }

    #[test]
    fn strategy_resolution() {
        // 缺省 -> 阈值轮转
        assert_eq!(
            resolve_strategy(None).unwrap(),
            db::STRATEGY_THRESHOLD_ROTATION
        );
        assert_eq!(
            resolve_strategy(Some("")).unwrap(),
            db::STRATEGY_THRESHOLD_ROTATION
        );
        assert_eq!(
            resolve_strategy(Some("model_match")).unwrap(),
            db::STRATEGY_MODEL_MATCH
        );
        let err = resolve_strategy(Some("random")).unwrap_err();
        assert!(err.contains("未知的路由策略"), "未知策略应报错: {err}");
    }
}
