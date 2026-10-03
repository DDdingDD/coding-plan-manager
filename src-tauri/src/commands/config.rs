//! 配置导入导出：把全部计划与聚合器配置（含绑定顺序、「当前计划」指定、令牌）
//! 导出为 JSON 文件，导入可复现配置。
//!
//! 导出格式用「文件内局部引用号」（ref_id / plan_refs / current_plan_ref）而非
//! 数据库自增 id——跨库导入时自增 id 必然变化，引用关系必须解耦才能复现。
//! 刻意不导出运行状态：created_at（导入时重新生成）、used_tokens（导入归零）、
//! 服务运行态（导入后一律停止）。聚合器 auth_token 原样保留——客户端的
//! ANTHROPIC_AUTH_TOKEN 指向它，不保留就复现不了客户端配置。

use crate::db;
use crate::state::{AppState, RunningServer};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const CONFIG_FORMAT: &str = "cpm-config";
const CONFIG_VERSION: i64 = 1;

// ---------------------------------------------------------------------------
// 导出文件结构（磁盘 JSON 用 snake_case 键，不经 invoke 传输；
// #[serde(default)] 兼容手工编辑/缺字段的文件）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedConfig {
    pub format: String,
    pub version: i64,
    #[serde(default)]
    pub exported_at: String,
    #[serde(default)]
    pub plans: Vec<PlanExport>,
    #[serde(default)]
    pub aggregators: Vec<AggregatorExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanExport {
    /// 文件内局部引用号（供 plan_refs / current_plan_ref 引用，导入时映射为新自增 id）
    pub ref_id: i64,
    pub name: String,
    pub base_url: String,
    pub auth_token: String,
    #[serde(default)]
    pub remark: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatorExport {
    pub name: String,
    pub port: i64,
    pub auth_token: String,
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default = "default_threshold")]
    pub token_threshold: i64,
    /// 绑定的计划引用（数组顺序即绑定顺序 / 轮转次序）
    #[serde(default)]
    pub plan_refs: Vec<i64>,
    /// 手动指定的「当前计划」（NULL = 未指定）
    #[serde(default)]
    pub current_plan_ref: Option<i64>,
}

fn default_true() -> bool {
    true
}

fn default_strategy() -> String {
    db::STRATEGY_THRESHOLD_ROTATION.to_string()
}

fn default_threshold() -> i64 {
    1_000_000
}

// ---------------------------------------------------------------------------
// 命令返回值
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ImportResult {
    pub plans_imported: usize,
    pub aggregators_imported: usize,
    /// 端口冲突自动改配的明细（old_port → new_port）
    pub port_changes: Vec<PortChange>,
    /// 替换导入时停止的运行中服务数
    pub stopped_servers: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortChange {
    pub name: String,
    pub old_port: i64,
    pub new_port: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// 合并：文件内容作为新条目追加，现有配置不动
    Merge,
    /// 替换：清空现有配置后按文件复现
    Replace,
}

fn parse_mode(s: &str) -> Result<ImportMode, String> {
    match s {
        "merge" => Ok(ImportMode::Merge),
        "replace" => Ok(ImportMode::Replace),
        _ => Err(format!("导入模式无效：{s}")),
    }
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

// ---------------------------------------------------------------------------
// 纯函数核心（无 Tauri 依赖，单测直接驱动）
// ---------------------------------------------------------------------------

/// 汇总当前库的全部配置为可序列化结构。
/// 聚合器 current_plan_id 由 db::list_aggregators 正常返回（#[serde(skip)]
/// 只影响序列化输出，读库不受影响）
fn build_export(conn: &Connection) -> rusqlite::Result<ExportedConfig> {
    let plans = db::list_plans(conn)?;
    // DB id -> ref_id 映射（ref_id 按计划顺序 1..n 编号）
    let mut id_to_ref: HashMap<i64, i64> = HashMap::new();
    let plan_exports: Vec<PlanExport> = plans
        .iter()
        .enumerate()
        .map(|(i, p)| {
            id_to_ref.insert(p.id, (i + 1) as i64);
            PlanExport {
                ref_id: (i + 1) as i64,
                name: p.name.clone(),
                base_url: p.base_url.clone(),
                auth_token: p.auth_token.clone(),
                remark: p.remark.clone(),
                models: p.models.clone(),
                enabled: p.enabled,
            }
        })
        .collect();

    let aggregators = db::list_aggregators(conn)?;
    let agg_exports: Vec<AggregatorExport> = aggregators
        .iter()
        .map(|a| {
            let bindings = db::list_bindings(conn, a.id)?;
            Ok(AggregatorExport {
                name: a.name.clone(),
                port: a.port,
                auth_token: a.auth_token.clone(),
                strategy: a.strategy.clone(),
                token_threshold: a.token_threshold,
                // list_bindings 按 position 排序，数组顺序即绑定顺序
                plan_refs: bindings
                    .iter()
                    .map(|b| id_to_ref.get(&b.plan.id).copied().unwrap_or(0))
                    .collect(),
                current_plan_ref: a
                    .current_plan_id
                    .and_then(|pid| id_to_ref.get(&pid).copied()),
            })
        })
        .collect::<rusqlite::Result<_>>()?;

    Ok(ExportedConfig {
        format: CONFIG_FORMAT.to_string(),
        version: CONFIG_VERSION,
        exported_at: db::now_str(),
        plans: plan_exports,
        aggregators: agg_exports,
    })
}

/// 导入前校验文件内容（中文错误带条目名上下文）。
/// 端口冲突不在校验层拒绝（DB 内或文件内重复统一由应用层自动改配，
/// 对 replace 模式的自相矛盾文件更宽容）
fn validate_import(cfg: &ExportedConfig) -> Result<(), String> {
    if cfg.format != CONFIG_FORMAT {
        return Err("这不是本应用导出的配置文件".into());
    }
    if cfg.version > CONFIG_VERSION {
        return Err(format!(
            "配置文件版本过新（v{}），请升级应用后再导入",
            cfg.version
        ));
    }
    let mut refs: HashSet<i64> = HashSet::new();
    for p in &cfg.plans {
        if p.name.trim().is_empty() {
            return Err("计划名称不能为空".into());
        }
        if !p.base_url.starts_with("http://") && !p.base_url.starts_with("https://") {
            return Err(format!(
                "计划 {} 的 BASE_URL 必须以 http:// 或 https:// 开头",
                p.name
            ));
        }
        if p.auth_token.trim().is_empty() {
            return Err(format!("计划 {} 的 AUTH_TOKEN 不能为空", p.name));
        }
        if !refs.insert(p.ref_id) {
            return Err(format!("计划 ref_id {} 重复", p.ref_id));
        }
    }
    for a in &cfg.aggregators {
        if a.name.trim().is_empty() {
            return Err("聚合器名称不能为空".into());
        }
        if !(1..=65535).contains(&a.port) {
            return Err(format!("聚合器 {} 的端口必须在 1-65535 之间", a.name));
        }
        if a.auth_token.trim().is_empty() {
            return Err(format!("聚合器 {} 的 AUTH_TOKEN 不能为空", a.name));
        }
        if !db::is_known_strategy(&a.strategy) {
            return Err(format!("聚合器 {} 的路由策略未知：{}", a.name, a.strategy));
        }
        if a.token_threshold < 1 {
            return Err(format!("聚合器 {} 的 token 阈值必须大于 0", a.name));
        }
        for r in &a.plan_refs {
            if !refs.contains(r) {
                return Err(format!(
                    "聚合器 {} 绑定了不存在的计划（ref_id {}）",
                    a.name, r
                ));
            }
        }
    }
    Ok(())
}

/// 决定导入聚合器的最终端口：Ok(None) = 保留原端口（未占用）；
/// Ok(Some(新)) = 冲突自动改配；端口段耗尽报错。
/// 保留原端口时不做 bind 实测（与 create_aggregator 对显式端口的行为一致）
fn resolve_import_port(wanted: i64, taken: &HashSet<i64>) -> Result<Option<i64>, String> {
    if !taken.contains(&wanted) {
        return Ok(None);
    }
    let free = crate::commands::aggregators::find_free_port(taken)
        .ok_or_else(|| "8300-8399 范围内没有可用端口".to_string())?;
    Ok(Some(free))
}

/// 执行导入（事务包裹：任一步失败整体回滚，不留半截数据）
fn apply_import(
    conn: &mut Connection,
    cfg: &ExportedConfig,
    mode: ImportMode,
) -> Result<ImportResult, String> {
    validate_import(cfg)?;
    let tx = conn.transaction().map_err(e2s)?;

    let mut taken: HashSet<i64> = db::list_aggregators(&tx)
        .map_err(e2s)?
        .iter()
        .map(|a| a.port)
        .collect();
    if mode == ImportMode::Replace {
        db::clear_config(&tx).map_err(e2s)?;
        taken.clear();
    }

    // 计划：按文件序插入，建 ref_id -> 新自增 id 映射
    let mut ref_map: HashMap<i64, i64> = HashMap::new();
    for p in &cfg.plans {
        let created = db::create_plan_full(
            &tx,
            &p.name,
            &p.base_url,
            &p.auth_token,
            &p.remark,
            &p.models,
            p.enabled,
        )
        .map_err(e2s)?;
        ref_map.insert(p.ref_id, created.id);
    }

    // 聚合器：端口冲突自动改配；绑定顺序复现；「当前计划」恢复
    let mut port_changes = Vec::new();
    for a in &cfg.aggregators {
        let port = match resolve_import_port(a.port, &taken)? {
            Some(new) => {
                port_changes.push(PortChange {
                    name: a.name.clone(),
                    old_port: a.port,
                    new_port: new,
                });
                new
            }
            None => a.port,
        };
        taken.insert(port);
        let agg = db::create_aggregator(
            &tx,
            &a.name,
            port,
            &a.auth_token,
            a.token_threshold,
            &a.strategy,
        )
        .map_err(e2s)?;
        // plan_refs 映射到新 id 并去重保序（重复绑定违反 UNIQUE 约束）
        let mut seen: HashSet<i64> = HashSet::new();
        let plan_ids: Vec<i64> = a
            .plan_refs
            .iter()
            .filter_map(|r| ref_map.get(r).copied())
            .filter(|pid| seen.insert(*pid))
            .collect();
        db::set_aggregator_plans(&tx, agg.id, &plan_ids).map_err(e2s)?;
        // 恢复「当前计划」：刻意走 db 层（无「绑定且启用」校验），忠实复现导出时的
        // 指定状态——允许指向禁用计划，失效场景由策略每请求重读配置时自愈。
        // 引用未绑定/未知的计划则忽略，留空
        if let Some(r) = a.current_plan_ref {
            if let Some(&pid) = ref_map.get(&r) {
                if plan_ids.contains(&pid) {
                    db::set_current_plan(&tx, agg.id, pid).map_err(e2s)?;
                }
            }
        }
    }

    let plans_imported = cfg.plans.len();
    let aggregators_imported = cfg.aggregators.len();
    tx.commit().map_err(e2s)?;
    Ok(ImportResult {
        plans_imported,
        aggregators_imported,
        port_changes,
        stopped_servers: 0,
    })
}

// ---------------------------------------------------------------------------
// Tauri 命令
// ---------------------------------------------------------------------------

/// 导出全部计划与聚合器配置到 JSON 文件。返回写入路径；用户取消返回 None。
/// 同步命令：rfd sync API 在 Windows 任意线程可用，macOS 要求主线程——
/// Tauri v2 同步命令恰在主线程执行，两端皆稳；文件对话框模态阻塞主线程是原生行为
#[tauri::command]
pub fn export_config(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    let default_name = format!(
        "cpm-config-{}.json",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let Some(mut path) = rfd::FileDialog::new()
        .add_filter("JSON 文件", &["json"])
        .set_file_name(&default_name)
        .save_file()
    else {
        return Ok(None); // 用户取消
    };
    if path.extension().is_none() {
        path.set_extension("json");
    }
    let conn = lock(&state);
    let cfg = build_export(&conn).map_err(e2s)?;
    drop(conn);
    let json =
        serde_json::to_string_pretty(&cfg).map_err(|e| format!("序列化配置失败: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("文件写入失败: {e}"))?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// 导入配置文件。mode："merge" 追加到现有配置 / "replace" 清空后复现；
/// 返回导入结果；用户取消返回 None
#[tauri::command]
pub fn import_config(
    state: tauri::State<'_, AppState>,
    mode: String,
) -> Result<Option<ImportResult>, String> {
    let mode = parse_mode(&mode)?;
    let Some(path) = rfd::FileDialog::new()
        .add_filter("JSON 文件", &["json"])
        .pick_file()
    else {
        return Ok(None); // 用户取消
    };
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("文件读取失败: {e}"))?;
    let cfg: ExportedConfig =
        serde_json::from_str(&raw).map_err(|e| format!("配置文件解析失败：{e}"))?;

    // 替换模式：先停止全部运行中的服务。fire-and-forget——优雅关停会等待长 SSE
    // 连接排水（可达数分钟），等待会冻结主线程；cancel 后句柄分离，任务后台自行
    // 结束。delete_aggregator 的 5s 超时等待同样不保证端口释放。锁序与之一致：
    // 先 servers 后 db，不嵌套持有
    let stopped_servers = if mode == ImportMode::Replace {
        let handles: Vec<RunningServer> = state
            .servers
            .lock()
            .unwrap()
            .drain()
            .map(|(_, h)| h)
            .collect();
        for h in &handles {
            h.cancel.cancel();
        }
        handles.len()
    } else {
        0
    };

    let mut conn = lock(&state);
    let mut result = apply_import(&mut conn, &cfg, mode)?;
    result.stopped_servers = stopped_servers;
    Ok(Some(result))
}

// ---------------------------------------------------------------------------
// 单元测试（纯函数路径，不弹对话框）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        db::init_db(":memory:").unwrap()
    }

    /// 标准样例文件：2 计划（p2 只给必填字段，顺带验证 serde 缺省值）+
    /// 1 聚合器（model_match、绑定顺序 [p2,p1]、当前计划指向 p2）
    fn sample_cfg() -> ExportedConfig {
        serde_json::from_str(
            r#"{
                "format": "cpm-config",
                "version": 1,
                "exported_at": "2026-10-03 00:00:00",
                "plans": [
                    {"ref_id": 1, "name": "p1", "base_url": "https://a.example",
                     "auth_token": "tok1", "remark": "r1", "models": ["glm-4.7"], "enabled": true},
                    {"ref_id": 2, "name": "p2", "base_url": "https://b.example", "auth_token": "tok2"}
                ],
                "aggregators": [
                    {"name": "g1", "port": 8300, "auth_token": "cpm-1",
                     "strategy": "model_match", "token_threshold": 5000,
                     "plan_refs": [2, 1], "current_plan_ref": 2}
                ]
            }"#,
        )
        .unwrap()
    }

    /// 源库导出 → JSON → 另一个预置无关数据的库替换导入 → 全字段复现
    #[test]
    fn export_round_trip_replace() {
        // 源库：p1 启用、p2 禁用带 models；聚合器绑定 [p2,p1]、当前计划指向禁用的 p2
        let src = mem();
        let p1 = db::create_plan(&src, "p1", "https://a.example", "tok1", "r1", &[]).unwrap();
        let p2 = db::create_plan(
            &src,
            "p2",
            "https://b.example",
            "tok2",
            "r2",
            &["glm-4.7".to_string()],
        )
        .unwrap();
        db::update_plan(
            &src,
            p2.id,
            "p2",
            "https://b.example",
            "tok2",
            "r2",
            false,
            &["glm-4.7".to_string()],
        )
        .unwrap();
        let agg = db::create_aggregator(&src, "g1", 8300, "cpm-src", 5000, db::STRATEGY_MODEL_MATCH)
            .unwrap();
        db::set_aggregator_plans(&src, agg.id, &[p2.id, p1.id]).unwrap();
        db::set_current_plan(&src, agg.id, p2.id).unwrap();
        // 制造非零用量，验证导入后运行状态归零
        let bs = db::list_bindings(&src, agg.id).unwrap();
        db::add_binding_usage(&src, bs[0].binding_id, 123).unwrap();

        let cfg = build_export(&src).unwrap();
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        // 运行状态不进文件（配置 vs 运行状态的边界）
        assert!(!json.contains("used_tokens"), "{json}");
        assert!(!json.contains("created_at"), "{json}");
        let cfg: ExportedConfig = serde_json::from_str(&json).unwrap();

        // 目标库预置无关数据：replace 导入后应被清空重建
        let mut dst = mem();
        db::create_plan(&dst, "old", "https://old.example", "old-tok", "", &[]).unwrap();
        db::create_aggregator(&dst, "oldg", 9100, "cpm-old", 100, db::STRATEGY_THRESHOLD_ROTATION)
            .unwrap();

        let result = apply_import(&mut dst, &cfg, ImportMode::Replace).unwrap();
        assert_eq!(result.plans_imported, 2);
        assert_eq!(result.aggregators_imported, 1);
        assert!(result.port_changes.is_empty());

        // 计划复现：enabled=false 保留、models 往返、新 id 与源库不同
        let plans = db::list_plans(&dst).unwrap();
        assert_eq!(plans.len(), 2);
        let np1 = plans.iter().find(|p| p.name == "p1").unwrap();
        let np2 = plans.iter().find(|p| p.name == "p2").unwrap();
        assert!(np1.enabled);
        assert_eq!(np1.auth_token, "tok1");
        assert!(!np2.enabled);
        assert_eq!(np2.models, vec!["glm-4.7".to_string()]);
        assert_ne!(np1.id, p1.id);

        // 聚合器复现：令牌原样、绑定顺序指向新 id、当前计划指向禁用计划、用量归零
        let aggs = db::list_aggregators(&dst).unwrap();
        assert_eq!(aggs.len(), 1);
        let na = &aggs[0];
        assert_eq!(na.name, "g1");
        assert_eq!(na.port, 8300);
        assert_eq!(na.auth_token, "cpm-src");
        assert_eq!(na.strategy, db::STRATEGY_MODEL_MATCH);
        assert_eq!(na.token_threshold, 5000);
        assert_eq!(na.current_plan_id, Some(np2.id));
        let bs = db::list_bindings(&dst, na.id).unwrap();
        assert_eq!(bs.len(), 2);
        assert_eq!(bs[0].plan.id, np2.id);
        assert_eq!(bs[0].position, 0);
        assert_eq!(bs[1].plan.id, np1.id);
        assert!(bs.iter().all(|b| b.used_tokens == 0));
    }

    /// 合并导入：现有配置原样保留，新条目的绑定引用映射后的新 id（而非同号旧计划）
    #[test]
    fn merge_preserves_existing() {
        let mut dst = mem();
        let old_p = db::create_plan(&dst, "old", "https://old.example", "old-tok", "", &[])
            .unwrap();
        db::create_aggregator(&dst, "oldg", 9000, "cpm-old", 100, db::STRATEGY_THRESHOLD_ROTATION)
            .unwrap();

        let cfg = sample_cfg();
        let result = apply_import(&mut dst, &cfg, ImportMode::Merge).unwrap();
        assert_eq!(result.plans_imported, 2);
        assert_eq!(result.aggregators_imported, 1);
        assert!(result.port_changes.is_empty());

        // 现有计划原样保留
        let plans = db::list_plans(&dst).unwrap();
        assert_eq!(plans.len(), 3);
        let got_old = plans.iter().find(|p| p.id == old_p.id).unwrap();
        assert_eq!(got_old.name, "old");
        assert_eq!(got_old.auth_token, "old-tok");

        // 绑定指向文件计划映射后的新 id，而不是同号旧计划（ref 映射碰撞防线）
        let aggs = db::list_aggregators(&dst).unwrap();
        let na = aggs.iter().find(|a| a.name == "g1").unwrap();
        let bs = db::list_bindings(&dst, na.id).unwrap();
        assert_eq!(bs.len(), 2);
        assert_ne!(bs[0].plan.id, old_p.id);
        assert_ne!(bs[1].plan.id, old_p.id);
        assert_ne!(bs[0].plan.id, bs[1].plan.id);
        // 缺省字段：p2 无 remark/models/enabled → 默认空、空、true
        let np2 = bs
            .iter()
            .map(|b| &b.plan)
            .find(|p| p.name == "p2")
            .unwrap();
        assert!(np2.enabled);
        assert!(np2.models.is_empty());
        assert_eq!(np2.remark, "");
    }

    /// 合并导入端口冲突：自动改配到 8300-8399 空闲端口并在结果中报告
    #[test]
    fn merge_port_conflict_auto_assigns() {
        let mut dst = mem();
        db::create_aggregator(&dst, "占用者", 8300, "cpm-x", 100, db::STRATEGY_THRESHOLD_ROTATION)
            .unwrap();

        let result = apply_import(&mut dst, &sample_cfg(), ImportMode::Merge).unwrap();
        assert_eq!(result.port_changes.len(), 1);
        let pc = &result.port_changes[0];
        assert_eq!(pc.name, "g1");
        assert_eq!(pc.old_port, 8300);
        assert!((8300..=8399).contains(&pc.new_port));
        assert_ne!(pc.new_port, 8300);
        // 落库端口与报告一致
        let aggs = db::list_aggregators(&dst).unwrap();
        assert!(aggs.iter().any(|a| a.name == "g1" && a.port == pc.new_port));
    }

    /// 替换导入：清空后原端口自然可用（不报冲突、不改配）
    #[test]
    fn replace_allows_reused_ports() {
        let mut dst = mem();
        db::create_aggregator(&dst, "oldg", 8300, "cpm-old", 100, db::STRATEGY_THRESHOLD_ROTATION)
            .unwrap();

        let result = apply_import(&mut dst, &sample_cfg(), ImportMode::Replace).unwrap();
        assert!(result.port_changes.is_empty());
        let aggs = db::list_aggregators(&dst).unwrap();
        assert_eq!(aggs.len(), 1);
        assert_eq!(aggs[0].name, "g1");
        assert_eq!(aggs[0].port, 8300);
    }

    /// 各类非法文件被拒绝，且中文错误带条目名上下文
    #[test]
    fn validation_rejects_bad_files() {
        let parse = |s: &str| serde_json::from_str::<ExportedConfig>(s).unwrap();
        let cases: Vec<(&str, &str)> = vec![
            (
                r#"{"format": "other", "version": 1, "exported_at": "", "plans": [], "aggregators": []}"#,
                "不是本应用导出",
            ),
            (
                r#"{"format": "cpm-config", "version": 999, "exported_at": "", "plans": [], "aggregators": []}"#,
                "版本过新",
            ),
            (
                r#"{"format": "cpm-config", "version": 1, "exported_at": "",
                    "plans": [{"ref_id": 1, "name": " ", "base_url": "https://a", "auth_token": "t"}],
                    "aggregators": []}"#,
                "名称不能为空",
            ),
            (
                r#"{"format": "cpm-config", "version": 1, "exported_at": "",
                    "plans": [{"ref_id": 1, "name": "p", "base_url": "ftp://a", "auth_token": "t"}],
                    "aggregators": []}"#,
                "http://",
            ),
            (
                r#"{"format": "cpm-config", "version": 1, "exported_at": "",
                    "plans": [{"ref_id": 1, "name": "p", "base_url": "https://a", "auth_token": "t"},
                              {"ref_id": 1, "name": "q", "base_url": "https://b", "auth_token": "t"}],
                    "aggregators": []}"#,
                "ref_id 1 重复",
            ),
            (
                r#"{"format": "cpm-config", "version": 1, "exported_at": "",
                    "plans": [{"ref_id": 1, "name": "p", "base_url": "https://a", "auth_token": "t"}],
                    "aggregators": [{"name": "g", "port": 0, "auth_token": "cpm", "strategy": "threshold_rotation", "token_threshold": 1, "plan_refs": []}]}"#,
                "1-65535",
            ),
            (
                r#"{"format": "cpm-config", "version": 1, "exported_at": "",
                    "plans": [{"ref_id": 1, "name": "p", "base_url": "https://a", "auth_token": "t"}],
                    "aggregators": [{"name": "g", "port": 8300, "auth_token": "cpm", "strategy": "unknown", "token_threshold": 1, "plan_refs": []}]}"#,
                "路由策略未知",
            ),
            (
                r#"{"format": "cpm-config", "version": 1, "exported_at": "",
                    "plans": [{"ref_id": 1, "name": "p", "base_url": "https://a", "auth_token": "t"}],
                    "aggregators": [{"name": "g", "port": 8300, "auth_token": "cpm", "strategy": "threshold_rotation", "token_threshold": 0, "plan_refs": []}]}"#,
                "阈值必须大于 0",
            ),
            (
                r#"{"format": "cpm-config", "version": 1, "exported_at": "",
                    "plans": [{"ref_id": 1, "name": "p", "base_url": "https://a", "auth_token": "t"}],
                    "aggregators": [{"name": "g", "port": 8300, "auth_token": "", "strategy": "threshold_rotation", "token_threshold": 1, "plan_refs": []}]}"#,
                "AUTH_TOKEN 不能为空",
            ),
            (
                r#"{"format": "cpm-config", "version": 1, "exported_at": "",
                    "plans": [],
                    "aggregators": [{"name": "g", "port": 8300, "auth_token": "cpm", "strategy": "threshold_rotation", "token_threshold": 1, "plan_refs": [7]}]}"#,
                "绑定了不存在的计划",
            ),
        ];
        for (json, keyword) in cases {
            let mut dst = mem();
            let err = apply_import(&mut dst, &parse(json), ImportMode::Merge)
                .expect_err(&format!("应被拒绝：{json}"));
            assert!(err.contains(keyword), "期望含「{keyword}」实际「{err}」");
            // 校验失败无副作用
            assert!(db::list_plans(&dst).unwrap().is_empty());
        }
        // 非 JSON 垃圾串在解析层被拒
        assert!(serde_json::from_str::<ExportedConfig>("垃圾文件").is_err());
    }

    /// current_plan_ref 引用了文件中存在但未绑定的计划 → 导入后留空
    #[test]
    fn current_plan_ref_ignored_when_unbound() {
        let mut dst = mem();
        let cfg: ExportedConfig = serde_json::from_str(
            r#"{
                "format": "cpm-config", "version": 1, "exported_at": "",
                "plans": [
                    {"ref_id": 1, "name": "p1", "base_url": "https://a.example", "auth_token": "tok1"},
                    {"ref_id": 2, "name": "p2", "base_url": "https://b.example", "auth_token": "tok2"}
                ],
                "aggregators": [
                    {"name": "g1", "port": 8300, "auth_token": "cpm-1",
                     "strategy": "model_match", "token_threshold": 5000,
                     "plan_refs": [1], "current_plan_ref": 2}
                ]
            }"#,
        )
        .unwrap();
        apply_import(&mut dst, &cfg, ImportMode::Merge).unwrap();
        let aggs = db::list_aggregators(&dst).unwrap();
        assert_eq!(aggs[0].current_plan_id, None);
    }

    /// 端口段耗尽：导入报错且事务回滚（已插入的计划一并撤销）
    #[test]
    fn port_exhaustion_rolls_back() {
        let mut dst = mem();
        for p in 8300..=8399 {
            db::create_aggregator(
                &dst,
                &format!("占{p}"),
                p,
                "cpm-x",
                100,
                db::STRATEGY_THRESHOLD_ROTATION,
            )
            .unwrap();
        }
        let before_plans = db::list_plans(&dst).unwrap().len();
        let before_aggs = db::list_aggregators(&dst).unwrap().len();

        let err = apply_import(&mut dst, &sample_cfg(), ImportMode::Merge).unwrap_err();
        assert!(err.contains("没有可用端口"), "{err}");
        // 事务回滚：两计划 + 一聚合器（冲突前的插入）全部撤销
        assert_eq!(db::list_plans(&dst).unwrap().len(), before_plans);
        assert_eq!(db::list_aggregators(&dst).unwrap().len(), before_aggs);
    }
}
