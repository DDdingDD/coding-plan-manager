use crate::body::{self, BodyStore};
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OptionalExtension, Result, ToSql};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 轮转策略标识：阈值轮转（按绑定顺序 + token 阈值）
pub const STRATEGY_THRESHOLD_ROTATION: &str = "threshold_rotation";
/// 轮转策略标识：模型匹配（按请求模型匹配计划的「支持模型」，粘性当前计划）
pub const STRATEGY_MODEL_MATCH: &str = "model_match";

/// 校验策略标识是否为已知值
pub fn is_known_strategy(s: &str) -> bool {
    s == STRATEGY_THRESHOLD_ROTATION || s == STRATEGY_MODEL_MATCH
}

/// 「支持模型」列的存储格式：逗号分隔的模型名原始串（读取时 split_models 解析）
fn join_models(models: &[String]) -> String {
    models
        .iter()
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

/// 解析「支持模型」列为模型名列表（trim + 去空，保持顺序）
fn split_models(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .map(|m| m.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodingPlan {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub auth_token: String,
    pub remark: String,
    /// 支持的模型名列表（空 = 未配置，模型匹配策略下不参与匹配）
    pub models: Vec<String>,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Aggregator {
    pub id: i64,
    pub name: String,
    pub port: i64,
    pub auth_token: String,
    pub strategy: String,
    pub token_threshold: i64,
    /// 「当前计划」：模型匹配策略的粘性路由目标；阈值轮转策略的手动固定计划
    /// （NULL = 未固定，自动轮转）。serde skip：AggregatorView 以 flatten 内嵌本结构
    /// 且自带派生的 current_plan_id / manual_current_plan_id，同时序列化会产生重复 JSON 键
    #[serde(skip)]
    pub current_plan_id: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingWithPlan {
    pub binding_id: i64,
    pub position: i64,
    pub used_tokens: i64,
    pub plan: CodingPlan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRow {
    pub id: i64,
    pub aggregator_id: i64,
    pub plan_id: Option<i64>,
    pub method: String,
    pub path: String,
    pub status: i64,
    pub request_body: String,
    pub response_body: String,
    pub model: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    pub duration_ms: i64,
    pub created_at: String,
    /// 冗余的关联名称，便于前端直接展示
    pub aggregator_name: Option<String>,
    pub plan_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMessage {
    pub aggregator_id: i64,
    pub plan_id: Option<i64>,
    pub method: String,
    pub path: String,
    pub status: i64,
    pub request_body: String,
    pub response_body: String,
    pub model: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    pub duration_ms: i64,
}

/// 消息列表行：不含请求/响应体两个大字段（各上限 2MB），
/// 列表/分页查询专用；完整消息经 get_message 拉取
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSummary {
    pub id: i64,
    pub aggregator_id: i64,
    pub plan_id: Option<i64>,
    pub method: String,
    pub path: String,
    pub status: i64,
    pub model: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    pub duration_ms: i64,
    pub created_at: String,
    /// 冗余的关联名称，便于前端直接展示
    pub aggregator_name: Option<String>,
    pub plan_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub requests: i64,
}

/// 小计里程：自上次手动重置以来的用量统计（started_at 为 None 表示从未重置，计入全部历史）
/// paused 为 true 时统计冻结在暂停时刻，暂停期间的用量不补计
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripStats {
    pub started_at: Option<String>,
    pub paused: bool,
    pub stats: UsageStats,
}

/// 分组统计的一个桶（按天 / 按小时 / 按模型），key 为日期、小时或模型名
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsBucket {
    pub key: String,
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub requests: i64,
}

// ---------------------------------------------------------------------------
// 初始化
// ---------------------------------------------------------------------------

/// 统计覆盖索引：五项 token 计数列入索引，统计查询全程不回表。
/// 背景：request_body/response_body（各上限 2MB）位于记录中部，表内读取其后的
/// 统计列必须走到溢出页链尾部，全表扫描成本正比于累计 body 字节而非行数。
/// stat_day/stat_model 纳入普通 created_at 列是覆盖判定的关键——GROUP BY/过滤
/// 表达式引用的原始列不在索引内时 planner 会判非覆盖而回表遍历溢出页链；
/// aggregator_id 打头使带聚合器过滤的查询获得「等值+日期范围+覆盖」完整前缀
/// （否则 planner 偏好 stat_agg 的等值 seek 而放弃覆盖）；表达式索引须与查询
/// 中的表达式逐字节一致才能命中。
const STAT_INDEX_DDL: &[(&str, &str)] = &[
    (
        "idx_messages_stat_agg",
        "CREATE INDEX idx_messages_stat_agg ON messages(aggregator_id, total_tokens, prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens)",
    ),
    (
        "idx_messages_stat_plan",
        "CREATE INDEX idx_messages_stat_plan ON messages(plan_id, total_tokens, prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens)",
    ),
    (
        "idx_messages_stat_time",
        "CREATE INDEX idx_messages_stat_time ON messages(created_at, total_tokens, prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens)",
    ),
    (
        "idx_messages_stat_day",
        "CREATE INDEX idx_messages_stat_day ON messages(aggregator_id, substr(created_at,1,10), substr(created_at,12,2), created_at, total_tokens, prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens)",
    ),
    (
        "idx_messages_stat_model",
        "CREATE INDEX idx_messages_stat_model ON messages(aggregator_id, model, substr(created_at,1,10), created_at, total_tokens, prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens)",
    ),
];

/// 「未压缩行」部分索引：只收录请求体仍为明文（TEXT，旧库写入）的行。新行一律以
/// BLOB 落库、不进本索引，日常写入零写放大。它让三件事不碰大表：
/// 是否还有待压缩行（O(1)）、待压缩行数（只数索引）、回填的游标扫描
/// （WHERE id>? AND typeof(...)='text' ORDER BY id → SEARCH 本索引）。
/// 表达式须与查询逐字节一致才能命中
const LEGACY_INDEX_DDL: &str =
    "CREATE INDEX idx_messages_legacy ON messages(id) WHERE typeof(request_body)='text'";

/// DDL 文本空白归一化后比较（sqlite_master 保留多行原文但会剥掉 IF NOT EXISTS）
fn norm_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 确保索引存在且定义与目标 DDL 一致：CREATE INDEX IF NOT EXISTS 不会更新已
/// 存在索引的定义，定义演进过（库里建过旧版形状）时须 DROP 重建
fn ensure_index(conn: &Connection, name: &str, ddl: &str) -> Result<()> {
    let have: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name=?1",
            params![name],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(h) = have.as_deref() {
        if norm_sql(h) == norm_sql(ddl) {
            return Ok(());
        }
        conn.execute(&format!("DROP INDEX {name}"), [])?;
    }
    conn.execute(ddl, [])?;
    Ok(())
}

pub fn init_db(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    init_schema(&conn)?;
    Ok(conn)
}

/// 建表/补列/建索引，作用于给定连接（init_db 与测试共用，便于预置旧库验证升级路径）
fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA foreign_keys=ON;
        -- WAL 下安全：崩溃不损坏库，最坏丢最后一次未 checkpoint 的事务
        PRAGMA synchronous=NORMAL;
        -- 64MB 页缓存（负数=KiB），加速索引扫描
        PRAGMA cache_size=-65536;
        -- GROUP BY/ORDER BY 的临时结构放内存。
        -- 注意 VACUUM 的临时库同样受它支配：整理文件前须临时改回 FILE（见 vacuum）
        PRAGMA temp_store=MEMORY;

        CREATE TABLE IF NOT EXISTS coding_plans (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT NOT NULL,
            base_url    TEXT NOT NULL,
            auth_token  TEXT NOT NULL,
            remark      TEXT NOT NULL DEFAULT '',
            models      TEXT NOT NULL DEFAULT '',
            enabled     INTEGER NOT NULL DEFAULT 1,
            created_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS aggregators (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            name            TEXT NOT NULL,
            port            INTEGER NOT NULL,
            auth_token      TEXT NOT NULL,
            strategy        TEXT NOT NULL DEFAULT 'threshold_rotation',
            token_threshold INTEGER NOT NULL DEFAULT 1000000,
            current_plan_id INTEGER,
            created_at      TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS aggregator_plans (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            aggregator_id INTEGER NOT NULL,
            plan_id       INTEGER NOT NULL,
            position      INTEGER NOT NULL DEFAULT 0,
            used_tokens   INTEGER NOT NULL DEFAULT 0,
            UNIQUE(aggregator_id, plan_id)
        );

        CREATE TABLE IF NOT EXISTS messages (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            aggregator_id     INTEGER NOT NULL,
            plan_id           INTEGER,
            method            TEXT NOT NULL DEFAULT '',
            path              TEXT NOT NULL DEFAULT '',
            status            INTEGER NOT NULL DEFAULT 0,
            request_body      TEXT NOT NULL DEFAULT '',
            response_body     TEXT NOT NULL DEFAULT '',
            model             TEXT NOT NULL DEFAULT '',
            prompt_tokens     INTEGER NOT NULL DEFAULT 0,
            completion_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
            total_tokens      INTEGER NOT NULL DEFAULT 0,
            duration_ms       INTEGER NOT NULL DEFAULT 0,
            created_at        TEXT NOT NULL,
            -- 前缀增量（见 body 模块）：请求体前缀取自 req_ref_id 行的请求体，
            -- 本行 request_body 只存其后 req_prefix_len 字节起始的后缀；NULL = 整条。
            -- request_body/response_body 列声明仍为 TEXT：编码后的 BLOB 原样存储，
            -- 读取按存储类区分（TEXT = 旧库明文）
            req_ref_id        INTEGER,
            req_prefix_len    INTEGER
        );

        CREATE INDEX IF NOT EXISTS idx_messages_agg   ON messages(aggregator_id, id DESC);
        -- 旧索引 idx_messages_plan/date/created 的前缀已被统计覆盖索引取代
        -- （缺统计列，保留只会增加写放大）
        DROP INDEX IF EXISTS idx_messages_plan;
        DROP INDEX IF EXISTS idx_messages_date;
        DROP INDEX IF EXISTS idx_messages_created;
        CREATE INDEX IF NOT EXISTS idx_agg_plans_agg  ON aggregator_plans(aggregator_id);

        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        ",
    )?;
    // 先补列后建索引：统计覆盖索引引用 cache_read/cache_creation/model 等迁移列，
    // 旧库若先 ensure_index 会因 no such column 直接失败——安装包在装过旧版的机器上
    // 打开即闪退的根因（残留旧 schema 的 cpm.db）
    migrate(conn)?;
    // 统计覆盖索引：定义演进时自动 DROP 重建（见 STAT_INDEX_DDL 注释）
    for (name, ddl) in STAT_INDEX_DDL {
        ensure_index(conn, name, ddl)?;
    }
    // 未压缩行部分索引（旧库首启在此建立：只读各行的记录头，不读 body 溢出页）
    ensure_index(conn, "idx_messages_legacy", LEGACY_INDEX_DDL)?;
    Ok(())
}

/// 旧库增量迁移：缺列则补（messages 的 model/缓存 token 与引用列、coding_plans 的
/// models、aggregators 的 current_plan_id）
fn migrate(conn: &Connection) -> Result<()> {
    let mut has: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('messages')")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for r in rows {
            has.push(r?);
        }
    }
    if !has.iter().any(|c| c == "model") {
        conn.execute(
            "ALTER TABLE messages ADD COLUMN model TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !has.iter().any(|c| c == "cache_read_tokens") {
        conn.execute(
            "ALTER TABLE messages ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has.iter().any(|c| c == "cache_creation_tokens") {
        conn.execute(
            "ALTER TABLE messages ADD COLUMN cache_creation_tokens INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    ensure_column(conn, "messages", "req_ref_id", "INTEGER")?;
    ensure_column(conn, "messages", "req_prefix_len", "INTEGER")?;
    ensure_column(conn, "coding_plans", "models", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "aggregators", "current_plan_id", "INTEGER")?;
    Ok(())
}

/// 表缺指定列则补（带类型与默认值的 ADD COLUMN 片段）。
/// 表本身不存在时跳过（由 init_db 的 CREATE TABLE IF NOT EXISTS 负责）。
fn ensure_column(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    let table_exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !table_exists {
        return Ok(());
    }
    let sql = format!("SELECT name FROM pragma_table_info('{table}') WHERE name=?1");
    let present = conn
        .query_row(&sql, params![column], |_| Ok(()))
        .optional()?
        .is_some();
    if !present {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"),
            [],
        )?;
    }
    Ok(())
}

pub fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

// ---------------------------------------------------------------------------
// Coding Plan
// ---------------------------------------------------------------------------

const PLAN_COLS: &str = "id, name, base_url, auth_token, remark, models, enabled, created_at";

fn row_to_plan(row: &rusqlite::Row) -> Result<CodingPlan> {
    Ok(CodingPlan {
        id: row.get(0)?,
        name: row.get(1)?,
        base_url: row.get(2)?,
        auth_token: row.get(3)?,
        remark: row.get(4)?,
        models: split_models(&row.get::<_, String>(5)?),
        enabled: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
    })
}

pub fn list_plans(conn: &Connection) -> Result<Vec<CodingPlan>> {
    let sql = format!("SELECT {PLAN_COLS} FROM coding_plans ORDER BY id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_plan)?;
    rows.collect()
}

pub fn get_plan(conn: &Connection, id: i64) -> Result<Option<CodingPlan>> {
    let sql = format!("SELECT {PLAN_COLS} FROM coding_plans WHERE id=?1");
    conn.query_row(&sql, params![id], row_to_plan).optional()
}

pub fn create_plan(
    conn: &Connection,
    name: &str,
    base_url: &str,
    auth_token: &str,
    remark: &str,
    models: &[String],
) -> Result<CodingPlan> {
    let created = now_str();
    let models_raw = join_models(models);
    conn.execute(
        "INSERT INTO coding_plans (name, base_url, auth_token, remark, models, enabled, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)",
        params![name, base_url, auth_token, remark, models_raw, created],
    )?;
    let id = conn.last_insert_rowid();
    Ok(CodingPlan {
        id,
        name: name.to_string(),
        base_url: base_url.to_string(),
        auth_token: auth_token.to_string(),
        remark: remark.to_string(),
        models: split_models(&models_raw),
        enabled: true,
        created_at: created,
    })
}

/// 导入用：与 create_plan 相同，但保留 enabled 状态（create_plan 固定 enabled=1）
pub fn create_plan_full(
    conn: &Connection,
    name: &str,
    base_url: &str,
    auth_token: &str,
    remark: &str,
    models: &[String],
    enabled: bool,
) -> Result<CodingPlan> {
    let created = now_str();
    let models_raw = join_models(models);
    conn.execute(
        "INSERT INTO coding_plans (name, base_url, auth_token, remark, models, enabled, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![name, base_url, auth_token, remark, models_raw, enabled as i64, created],
    )?;
    let id = conn.last_insert_rowid();
    Ok(CodingPlan {
        id,
        name: name.to_string(),
        base_url: base_url.to_string(),
        auth_token: auth_token.to_string(),
        remark: remark.to_string(),
        models: split_models(&models_raw),
        enabled,
        created_at: created,
    })
}

pub fn update_plan(
    conn: &Connection,
    id: i64,
    name: &str,
    base_url: &str,
    auth_token: &str,
    remark: &str,
    enabled: bool,
    models: &[String],
) -> Result<Option<CodingPlan>> {
    let n = conn.execute(
        "UPDATE coding_plans SET name=?2, base_url=?3, auth_token=?4, remark=?5, models=?6, enabled=?7 WHERE id=?1",
        params![id, name, base_url, auth_token, remark, join_models(models), enabled as i64],
    )?;
    if n == 0 {
        return Ok(None);
    }
    get_plan(conn, id)
}

/// 删除计划，并级联解除其在所有聚合器下的绑定
pub fn delete_plan(conn: &Connection, id: i64) -> Result<usize> {
    conn.execute("DELETE FROM aggregator_plans WHERE plan_id=?1", params![id])?;
    let n = conn.execute("DELETE FROM coding_plans WHERE id=?1", params![id])?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Aggregator
// ---------------------------------------------------------------------------

const AGG_COLS: &str =
    "id, name, port, auth_token, strategy, token_threshold, current_plan_id, created_at";

fn row_to_agg(row: &rusqlite::Row) -> Result<Aggregator> {
    Ok(Aggregator {
        id: row.get(0)?,
        name: row.get(1)?,
        port: row.get(2)?,
        auth_token: row.get(3)?,
        strategy: row.get(4)?,
        token_threshold: row.get(5)?,
        current_plan_id: row.get(6)?,
        created_at: row.get(7)?,
    })
}

pub fn list_aggregators(conn: &Connection) -> Result<Vec<Aggregator>> {
    let sql = format!("SELECT {AGG_COLS} FROM aggregators ORDER BY id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_agg)?;
    rows.collect()
}

pub fn get_aggregator(conn: &Connection, id: i64) -> Result<Option<Aggregator>> {
    let sql = format!("SELECT {AGG_COLS} FROM aggregators WHERE id=?1");
    conn.query_row(&sql, params![id], row_to_agg).optional()
}

pub fn create_aggregator(
    conn: &Connection,
    name: &str,
    port: i64,
    auth_token: &str,
    token_threshold: i64,
    strategy: &str,
) -> Result<Aggregator> {
    let created = now_str();
    conn.execute(
        "INSERT INTO aggregators (name, port, auth_token, strategy, token_threshold, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![name, port, auth_token, strategy, token_threshold, created],
    )?;
    let id = conn.last_insert_rowid();
    Ok(Aggregator {
        id,
        name: name.to_string(),
        port,
        auth_token: auth_token.to_string(),
        strategy: strategy.to_string(),
        token_threshold,
        current_plan_id: None,
        created_at: created,
    })
}

pub fn update_aggregator(
    conn: &Connection,
    id: i64,
    name: &str,
    port: i64,
    token_threshold: i64,
    strategy: &str,
) -> Result<Option<Aggregator>> {
    let n = conn.execute(
        "UPDATE aggregators SET name=?2, port=?3, token_threshold=?4, strategy=?5 WHERE id=?1",
        params![id, name, port, token_threshold, strategy],
    )?;
    if n == 0 {
        return Ok(None);
    }
    get_aggregator(conn, id)
}

/// 设置「当前计划」（模型匹配=粘性路由目标；阈值轮转=手动固定，失效后由策略自动清除）。
/// 返回 false 表示聚合器不存在；plan 是否有效由调用方校验。
pub fn set_current_plan(conn: &Connection, id: i64, plan_id: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE aggregators SET current_plan_id=?2 WHERE id=?1",
        params![id, plan_id],
    )?;
    Ok(n > 0)
}

/// 清除「当前计划」指定（阈值轮转恢复自动轮转；模型匹配回退首个启用绑定并固化）
pub fn clear_current_plan(conn: &Connection, id: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE aggregators SET current_plan_id=NULL WHERE id=?1",
        params![id],
    )?;
    Ok(n > 0)
}

pub fn delete_aggregator(conn: &Connection, id: i64) -> Result<usize> {
    conn.execute(
        "DELETE FROM aggregator_plans WHERE aggregator_id=?1",
        params![id],
    )?;
    let n = conn.execute("DELETE FROM aggregators WHERE id=?1", params![id])?;
    Ok(n)
}

/// 导入（替换模式）用：清空全部配置表（绑定 → 聚合器 → 计划）。
/// messages 与 settings 不受影响（历史记录与小计里程语义独立于配置）
pub fn clear_config(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM aggregator_plans", [])?;
    conn.execute("DELETE FROM aggregators", [])?;
    conn.execute("DELETE FROM coding_plans", [])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 聚合器 <-> 计划 绑定
// ---------------------------------------------------------------------------

pub fn list_bindings(conn: &Connection, aggregator_id: i64) -> Result<Vec<BindingWithPlan>> {
    let mut stmt = conn.prepare(
        "SELECT ap.id, ap.position, ap.used_tokens,
                p.id, p.name, p.base_url, p.auth_token, p.remark, p.models, p.enabled, p.created_at
         FROM aggregator_plans ap
         JOIN coding_plans p ON p.id = ap.plan_id
         WHERE ap.aggregator_id=?1
         ORDER BY ap.position, ap.id",
    )?;
    let rows = stmt.query_map(params![aggregator_id], |row| {
        Ok(BindingWithPlan {
            binding_id: row.get(0)?,
            position: row.get(1)?,
            used_tokens: row.get(2)?,
            plan: CodingPlan {
                id: row.get(3)?,
                name: row.get(4)?,
                base_url: row.get(5)?,
                auth_token: row.get(6)?,
                remark: row.get(7)?,
                models: split_models(&row.get::<_, String>(8)?),
                enabled: row.get::<_, i64>(9)? != 0,
                created_at: row.get(10)?,
            },
        })
    })?;
    rows.collect()
}

/// 重设聚合器绑定的计划（按传入顺序决定轮转次序），已存在的绑定保留 used_tokens
pub fn set_aggregator_plans(conn: &Connection, aggregator_id: i64, plan_ids: &[i64]) -> Result<()> {
    // 先取当前 used_tokens 映射
    let mut old_usage = std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT plan_id, used_tokens FROM aggregator_plans WHERE aggregator_id=?1")?;
        let rows = stmt.query_map(params![aggregator_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        for r in rows {
            let (pid, used) = r?;
            old_usage.insert(pid, used);
        }
    }

    conn.execute(
        "DELETE FROM aggregator_plans WHERE aggregator_id=?1",
        params![aggregator_id],
    )?;
    for (idx, pid) in plan_ids.iter().enumerate() {
        let used = old_usage.get(pid).copied().unwrap_or(0);
        conn.execute(
            "INSERT INTO aggregator_plans (aggregator_id, plan_id, position, used_tokens)
             VALUES (?1, ?2, ?3, ?4)",
            params![aggregator_id, pid, idx as i64, used],
        )?;
    }
    Ok(())
}

/// 累加绑定用量，返回影响行数（0 表示绑定已不存在，如转发途中被重新绑定）
pub fn add_binding_usage(conn: &Connection, binding_id: i64, tokens: i64) -> Result<usize> {
    conn.execute(
        "UPDATE aggregator_plans SET used_tokens = used_tokens + ?2 WHERE id=?1",
        params![binding_id, tokens],
    )
}

/// 将聚合器下所有绑定的已用 token 清零
pub fn reset_binding_usage(conn: &Connection, aggregator_id: i64) -> Result<usize> {
    conn.execute(
        "UPDATE aggregator_plans SET used_tokens=0 WHERE aggregator_id=?1",
        params![aggregator_id],
    )
}

// ---------------------------------------------------------------------------
// 统计
// ---------------------------------------------------------------------------

const STATS_SELECT: &str = "SELECT COALESCE(SUM(total_tokens),0), COALESCE(SUM(prompt_tokens),0),
        COALESCE(SUM(completion_tokens),0), COALESCE(SUM(cache_read_tokens),0),
        COALESCE(SUM(cache_creation_tokens),0), COUNT(*)
 FROM messages";

fn row_to_usage(row: &rusqlite::Row) -> Result<UsageStats> {
    Ok(UsageStats {
        total_tokens: row.get(0)?,
        prompt_tokens: row.get(1)?,
        completion_tokens: row.get(2)?,
        cache_read_tokens: row.get(3)?,
        cache_creation_tokens: row.get(4)?,
        requests: row.get(5)?,
    })
}

fn stats_query(
    conn: &Connection,
    where_clause: &str,
    param: Option<&dyn ToSql>,
) -> Result<UsageStats> {
    let sql = format!("{STATS_SELECT} {where_clause}");
    match param {
        Some(v) => conn.query_row(&sql, params![v], row_to_usage),
        None => conn.query_row(&sql, [], row_to_usage),
    }
}

pub fn aggregator_stats(conn: &Connection, aggregator_id: i64) -> Result<UsageStats> {
    stats_query(conn, "WHERE aggregator_id=?1", Some(&aggregator_id))
}

pub fn plan_stats(conn: &Connection, plan_id: i64) -> Result<UsageStats> {
    stats_query(conn, "WHERE plan_id=?1", Some(&plan_id))
}

pub fn global_stats(conn: &Connection) -> Result<UsageStats> {
    stats_query(conn, "", None)
}

// ---------------------------------------------------------------------------
// 小计里程（trip meter）
// ---------------------------------------------------------------------------

/// settings 表中记录小计起点时间的键，值为 created_at 同格式的本地时间串
pub const SETTING_TRIP_STARTED_AT: &str = "trip_started_at";
/// 暂停时刻（键存在即处于暂停中，统计冻结在该时刻之前）
pub const SETTING_TRIP_PAUSED_AT: &str = "trip_paused_at";
/// 已闭合暂停区间的 JSON 数组 [[start, end], ...]，半开区间语义
pub const SETTING_TRIP_PAUSES: &str = "trip_pauses";

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM settings WHERE key=?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn delete_setting(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM settings WHERE key=?1", params![key])?;
    Ok(())
}

/// 已闭合的暂停区间列表（JSON 损坏时按无区间处理，避免统计整体崩掉）
fn get_trip_pauses(conn: &Connection) -> Result<Vec<(String, String)>> {
    Ok(match get_setting(conn, SETTING_TRIP_PAUSES)? {
        Some(s) => serde_json::from_str(&s).unwrap_or_default(),
        None => Vec::new(),
    })
}

/// 小计统计：从未重置（起点为 NULL）时计入全部历史；暂停区间的用量不计入；
/// 暂停中冻结在暂停时刻（暂停期间的用量即使恢复后也不补计）
pub fn trip_stats(conn: &Connection) -> Result<TripStats> {
    let started_at = get_setting(conn, SETTING_TRIP_STARTED_AT)?;
    let paused_at = get_setting(conn, SETTING_TRIP_PAUSED_AT)?;
    let pauses = get_trip_pauses(conn)?;

    let mut conds: Vec<String> = Vec::new();
    let mut values: Vec<String> = Vec::new();
    if let Some(since) = &started_at {
        values.push(since.clone());
        conds.push(format!("created_at>=?{}", values.len()));
    }
    for (start, end) in &pauses {
        // 半开区间 [start, end)：暂停瞬间的消息不计，恢复瞬间的计
        values.push(start.clone());
        values.push(end.clone());
        conds.push(format!(
            "(created_at<?{} OR created_at>=?{})",
            values.len() - 1,
            values.len()
        ));
    }
    if let Some(pa) = &paused_at {
        values.push(pa.clone());
        conds.push(format!("created_at<?{}", values.len()));
    }
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conds.join(" AND "))
    };
    let stats = conn.query_row(
        &format!("{STATS_SELECT} {where_clause}"),
        rusqlite::params_from_iter(values.iter()),
        row_to_usage,
    )?;
    Ok(TripStats {
        started_at,
        paused: paused_at.is_some(),
        stats,
    })
}

/// 暂停/继续小计：暂停时记暂停时刻；继续时把 [paused_at, now) 并入闭合区间
pub fn toggle_trip_pause(conn: &Connection) -> Result<TripStats> {
    match get_setting(conn, SETTING_TRIP_PAUSED_AT)? {
        None => set_setting(conn, SETTING_TRIP_PAUSED_AT, &now_str())?,
        Some(paused_at) => {
            let mut pauses = get_trip_pauses(conn)?;
            pauses.push((paused_at, now_str()));
            let json = serde_json::to_string(&pauses)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            set_setting(conn, SETTING_TRIP_PAUSES, &json)?;
            delete_setting(conn, SETTING_TRIP_PAUSED_AT)?;
        }
    }
    trip_stats(conn)
}

/// 重置小计：起点改为当前时间并清除暂停状态（不改任何消息数据，累计统计不受影响）
pub fn reset_trip(conn: &Connection) -> Result<TripStats> {
    set_setting(conn, SETTING_TRIP_STARTED_AT, &now_str())?;
    delete_setting(conn, SETTING_TRIP_PAUSED_AT)?;
    delete_setting(conn, SETTING_TRIP_PAUSES)?;
    trip_stats(conn)
}

// ---------------------------------------------------------------------------
// 分组统计（按天 / 按小时 / 按模型）
// ---------------------------------------------------------------------------

fn row_to_bucket(row: &rusqlite::Row) -> Result<StatsBucket> {
    Ok(StatsBucket {
        key: row.get(0)?,
        total_tokens: row.get(1)?,
        prompt_tokens: row.get(2)?,
        completion_tokens: row.get(3)?,
        cache_read_tokens: row.get(4)?,
        cache_creation_tokens: row.get(5)?,
        requests: row.get(6)?,
    })
}

/// 通用分组统计：key_sql 为分组表达式（天/小时/模型），可选聚合器过滤。
/// ordering 为 ORDER BY 表达式（小时/模型分组与 key 顺序不同）。
/// 过滤值一律走参数绑定，占位符序号由调用方在 where_clause 中与 params 一一对应。
fn bucket_query(
    conn: &Connection,
    key_sql: &str,
    where_clause: &str,
    ordering: &str,
    bind: &[&dyn ToSql],
) -> Result<Vec<StatsBucket>> {
    let sql = format!(
        "SELECT {key_sql} AS k, COALESCE(SUM(total_tokens),0), COALESCE(SUM(prompt_tokens),0), \
         COALESCE(SUM(completion_tokens),0), COALESCE(SUM(cache_read_tokens),0), \
         COALESCE(SUM(cache_creation_tokens),0), COUNT(*) \
         FROM messages {where_clause} GROUP BY k ORDER BY {ordering}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(bind, row_to_bucket)?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

/// 按天统计：最近 days 天（含今天），created_at 为本地时间 "YYYY-MM-DD HH:MM:SS"
pub fn stats_daily(
    conn: &Connection,
    aggregator_id: Option<i64>,
    days: i64,
) -> Result<Vec<StatsBucket>> {
    let cutoff = (chrono::Local::now() - chrono::Duration::days(days - 1))
        .format("%Y-%m-%d")
        .to_string();
    let (where_clause, bind): (&str, Vec<&dyn ToSql>) = match aggregator_id.as_ref() {
        Some(id) => (
            "WHERE aggregator_id=?1 AND substr(created_at,1,10)>=?2",
            vec![id, &cutoff],
        ),
        None => ("WHERE substr(created_at,1,10)>=?1", vec![&cutoff]),
    };
    bucket_query(conn, "substr(created_at,1,10)", where_clause, "k", &bind)
}

/// 按小时统计：指定日期（"YYYY-MM-DD"）内 0-23 点各小时用量
pub fn stats_hourly(
    conn: &Connection,
    aggregator_id: Option<i64>,
    date: &str,
) -> Result<Vec<StatsBucket>> {
    let (where_clause, bind): (&str, Vec<&dyn ToSql>) = match aggregator_id.as_ref() {
        Some(id) => (
            "WHERE aggregator_id=?1 AND substr(created_at,1,10)=?2",
            vec![id, &date],
        ),
        None => ("WHERE substr(created_at,1,10)=?1", vec![&date]),
    };
    bucket_query(
        conn,
        "substr(created_at,12,2)",
        where_clause,
        "CAST(k AS INTEGER)",
        &bind,
    )
}

/// 按模型统计：按总 token 倒序；days 为 Some 时仅统计近 days 天（含今天）
pub fn stats_by_model(
    conn: &Connection,
    aggregator_id: Option<i64>,
    days: Option<i64>,
) -> Result<Vec<StatsBucket>> {
    let cutoff = days.map(|d| {
        (chrono::Local::now() - chrono::Duration::days(d - 1))
            .format("%Y-%m-%d")
            .to_string()
    });

    let mut where_parts: Vec<String> = Vec::new();
    let mut bind: Vec<&dyn ToSql> = Vec::new();
    if let Some(id) = aggregator_id.as_ref() {
        bind.push(id);
        where_parts.push(format!("aggregator_id=?{}", bind.len()));
    }
    if let Some(c) = cutoff.as_ref() {
        bind.push(c);
        where_parts.push(format!("substr(created_at,1,10)>=?{}", bind.len()));
    }
    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_parts.join(" AND "))
    };
    bucket_query(
        conn,
        "model",
        &where_clause,
        "COALESCE(SUM(total_tokens),0) DESC, k",
        &bind,
    )
}

// ---------------------------------------------------------------------------
// 消息
// ---------------------------------------------------------------------------

/// get_message 的列集。两个 body 列排在统计列之前，引用两列追加在末尾（旧库经
/// ALTER 补列后的物理顺序与此一致）
const MSG_COLS: &str = "m.id, m.aggregator_id, m.plan_id, m.method, m.path, m.status, \
                        m.request_body, m.response_body, m.model, m.prompt_tokens, \
                        m.completion_tokens, m.cache_read_tokens, m.cache_creation_tokens, \
                        m.total_tokens, m.duration_ms, m.created_at, \
                        a.name, p.name, m.req_ref_id, m.req_prefix_len";

/// 落库的 body 列：原始字节 + 存储类是否为 BLOB（已编码；TEXT 为旧库明文）
#[derive(Debug, Clone)]
struct StoredBody {
    bytes: Vec<u8>,
    encoded: bool,
}

impl StoredBody {
    /// 取一行的第 idx 列。body 列只能这样按原始字节读：`row.get::<_, String>`
    /// 遇到 BLOB 会报 InvalidColumnType
    fn from_row(row: &rusqlite::Row, idx: usize) -> Result<Self> {
        let (bytes, encoded) = match row.get_ref(idx)? {
            ValueRef::Text(b) => (b.to_vec(), false),
            ValueRef::Blob(b) => (b.to_vec(), true),
            // 两列均为 NOT NULL；万一出现 NULL/数值按空体处理，不让整条详情失败
            _ => (Vec::new(), false),
        };
        Ok(Self { bytes, encoded })
    }

    /// 解码为落库前的字节：编码过的走 body::decode，明文原样返回
    fn decode(&self) -> std::result::Result<Vec<u8>, String> {
        if self.encoded {
            body::decode(&self.bytes)
        } else {
            Ok(self.bytes.clone())
        }
    }

    /// 解码为落库前的文本；数据损坏时正文位置给出中文标记
    fn to_text(&self) -> String {
        match self.decode() {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => format!("…[{e}]\n"),
        }
    }
}

/// get_message 的原始行：两个 body 保持落库形态，由 to_message_row 解码
struct RawMessage {
    id: i64,
    aggregator_id: i64,
    plan_id: Option<i64>,
    method: String,
    path: String,
    status: i64,
    request_body: StoredBody,
    response_body: StoredBody,
    model: String,
    prompt_tokens: i64,
    completion_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
    total_tokens: i64,
    duration_ms: i64,
    created_at: String,
    aggregator_name: Option<String>,
    plan_name: Option<String>,
    req_ref_id: Option<i64>,
    req_prefix_len: Option<i64>,
}

fn row_to_raw(row: &rusqlite::Row) -> Result<RawMessage> {
    Ok(RawMessage {
        id: row.get(0)?,
        aggregator_id: row.get(1)?,
        plan_id: row.get(2)?,
        method: row.get(3)?,
        path: row.get(4)?,
        status: row.get(5)?,
        request_body: StoredBody::from_row(row, 6)?,
        response_body: StoredBody::from_row(row, 7)?,
        model: row.get(8)?,
        prompt_tokens: row.get(9)?,
        completion_tokens: row.get(10)?,
        cache_read_tokens: row.get(11)?,
        cache_creation_tokens: row.get(12)?,
        total_tokens: row.get(13)?,
        duration_ms: row.get(14)?,
        created_at: row.get(15)?,
        aggregator_name: row.get(16)?,
        plan_name: row.get(17)?,
        req_ref_id: row.get(18)?,
        req_prefix_len: row.get(19)?,
    })
}

/// 引用链回溯的跳数上限：正常数据链深不超过 body::MAX_DEPTH，超限说明库被异常改动
/// （成环等），到顶即按引用断裂处理
const MAX_REF_HOPS: usize = 256;

/// 按主键取来源行的请求体落库形态及其引用信息（引用链回溯用）。
/// 只读到 request_body 为止的溢出页，不碰其后的 response_body
fn fetch_request(conn: &Connection, id: i64) -> Result<Option<(StoredBody, Option<i64>, Option<i64>)>> {
    conn.query_row(
        "SELECT request_body, req_ref_id, req_prefix_len FROM messages WHERE id=?1",
        params![id],
        |row| {
            Ok((
                StoredBody::from_row(row, 0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        },
    )
    .optional()
}

/// 还原一条消息的完整请求体。
///
/// 增量行的 request_body 只是相对来源行（req_ref_id）请求体前 req_prefix_len 字节的
/// 新增后缀：自本行向根回溯，逐层收集 (来源行, 前缀长度, 本层后缀)，再自根向叶
/// 「截到前缀长度 + 接后缀」拼接。引用断裂（来源行已不在、数据损坏、长度对不上、
/// 链过长）时不报错：正文以中文标记开头，其后是仍可还原的后缀部分
fn resolve_request(
    conn: &Connection,
    req: &StoredBody,
    ref_id: Option<i64>,
    prefix_len: Option<i64>,
) -> Result<String> {
    // levels[0] 为本行所在层，依次向上；每项 (来源行 id, 前缀长度, 本层自有后缀)
    let mut levels: Vec<(i64, usize, Vec<u8>)> = Vec::new();
    let mut cur = req.decode();
    let mut cur_ref = (ref_id, prefix_len);
    // 断裂说明（None = 链完整）
    let mut note: Option<String> = None;
    // 根行（整条存储）的完整请求体
    let mut root: Option<Vec<u8>> = None;
    while root.is_none() {
        let own = match std::mem::replace(&mut cur, Ok(Vec::new())) {
            Ok(b) => b,
            Err(e) => {
                note = Some(format!("…[{e}]\n"));
                break;
            }
        };
        match cur_ref {
            (None, _) => root = Some(own),
            (Some(pid), plen) => {
                let plen = plen.unwrap_or(0).max(0) as usize;
                levels.push((pid, plen, own));
                if levels.len() > MAX_REF_HOPS {
                    note = Some(format!(
                        "…[引用链过长（超过 {MAX_REF_HOPS} 层），前 {plen} 字节不可用]\n"
                    ));
                    break;
                }
                match fetch_request(conn, pid)? {
                    Some((body, r, p)) => {
                        cur = body.decode();
                        cur_ref = (r, p);
                    }
                    None => {
                        note = Some(format!(
                            "…[引用的请求体 #{pid} 已不存在，前 {plen} 字节无法恢复]\n"
                        ));
                        break;
                    }
                }
            }
        }
    }

    // 自根向叶拼接；断裂时 acc 从空开始，能还原多少后缀就给多少
    let mut acc = root.unwrap_or_default();
    for &(pid, plen, ref suffix) in levels.iter().rev() {
        if note.is_none() && plen > acc.len() {
            note = Some(format!(
                "…[引用的请求体 #{pid} 比记录的前缀长度短，前 {plen} 字节不完整]\n"
            ));
        }
        acc.truncate(plen.min(acc.len()));
        acc.extend_from_slice(suffix);
    }
    let mut out = String::new();
    if let Some(n) = note {
        out.push_str(&n);
    }
    out.push_str(&String::from_utf8_lossy(&acc));
    Ok(out)
}

fn to_message_row(conn: &Connection, raw: RawMessage) -> Result<MessageRow> {
    let RawMessage {
        id,
        aggregator_id,
        plan_id,
        method,
        path,
        status,
        request_body,
        response_body,
        model,
        prompt_tokens,
        completion_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        total_tokens,
        duration_ms,
        created_at,
        aggregator_name,
        plan_name,
        req_ref_id,
        req_prefix_len,
    } = raw;
    Ok(MessageRow {
        id,
        aggregator_id,
        plan_id,
        method,
        path,
        status,
        request_body: resolve_request(conn, &request_body, req_ref_id, req_prefix_len)?,
        response_body: response_body.to_text(),
        model,
        prompt_tokens,
        completion_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        total_tokens,
        duration_ms,
        created_at,
        aggregator_name,
        plan_name,
    })
}

/// 列表查询的列集：不含 request_body/response_body（各上限 2MB，
/// 列表页不展示，搬运它们只会白白撑大 IPC 负载）
const MSG_SUMMARY_COLS: &str = "m.id, m.aggregator_id, m.plan_id, m.method, m.path, m.status, \
                                m.model, m.prompt_tokens, m.completion_tokens, \
                                m.cache_read_tokens, m.cache_creation_tokens, m.total_tokens, \
                                m.duration_ms, m.created_at, a.name, p.name";

fn row_to_summary(row: &rusqlite::Row) -> Result<MessageSummary> {
    Ok(MessageSummary {
        id: row.get(0)?,
        aggregator_id: row.get(1)?,
        plan_id: row.get(2)?,
        method: row.get(3)?,
        path: row.get(4)?,
        status: row.get(5)?,
        model: row.get(6)?,
        prompt_tokens: row.get(7)?,
        completion_tokens: row.get(8)?,
        cache_read_tokens: row.get(9)?,
        cache_creation_tokens: row.get(10)?,
        total_tokens: row.get(11)?,
        duration_ms: row.get(12)?,
        created_at: row.get(13)?,
        aggregator_name: row.get(14)?,
        plan_name: row.get(15)?,
    })
}

/// 落库消息，返回 (行 id, 落库时间串)。created_at 是记录最后一列（在两个大 body
/// 之后），回读它同样要遍历溢出页链，故随返回值带出供调用方构造事件摘要。
///
/// 两个 body 按 body 模块的格式编码后落库；请求体相对近期同聚合器的请求体只存
/// 新增后缀（前缀增量）。store 是近期请求体缓存，须在持有数据库锁期间调用，
/// 锁序恒为 DB → store
pub fn insert_message(
    conn: &Connection,
    msg: &NewMessage,
    store: &BodyStore,
) -> Result<(i64, String)> {
    let created = now_str();
    let req = msg.request_body.as_bytes();
    let mut enc = body::Encoder::new(body::ONLINE_LEVEL);
    // 来源行落库前再确认仍在库中（与清空消息同处持锁区间，正常不会落空）
    let mut depth = 0;
    let (req_blob, ref_id, prefix_len) = match store.plan_delta(msg.aggregator_id, req) {
        Some(d) if ref_exists(conn, d.ref_id, msg.aggregator_id)? => {
            depth = d.depth;
            (
                enc.encode(&req[d.prefix_len..]),
                Some(d.ref_id),
                Some(d.prefix_len as i64),
            )
        }
        _ => (enc.encode(req), None, None),
    };
    let resp_blob = enc.encode(msg.response_body.as_bytes());
    conn.execute(
        "INSERT INTO messages (aggregator_id, plan_id, method, path, status, request_body,
                               response_body, model, prompt_tokens, completion_tokens,
                               cache_read_tokens, cache_creation_tokens,
                               total_tokens, duration_ms, created_at,
                               req_ref_id, req_prefix_len)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
        params![
            msg.aggregator_id,
            msg.plan_id,
            msg.method,
            msg.path,
            msg.status,
            req_blob,
            resp_blob,
            msg.model,
            msg.prompt_tokens,
            msg.completion_tokens,
            msg.cache_read_tokens,
            msg.cache_creation_tokens,
            msg.total_tokens,
            msg.duration_ms,
            &created,
            ref_id,
            prefix_len
        ],
    )?;
    let id = conn.last_insert_rowid();
    store.push(msg.aggregator_id, id, Arc::from(req), depth);
    Ok((id, created))
}

/// 引用来源行是否仍在库中且属于同一聚合器。引用只在同聚合器内建立，
/// 因此按聚合器清空消息删掉的一定是完整的引用闭包
fn ref_exists(conn: &Connection, id: i64, aggregator_id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM messages WHERE id=?1 AND aggregator_id=?2",
            params![id, aggregator_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// 点查聚合器/计划名（两表主键查找、小表开销为零），供事件摘要构造冗余展示名；
/// 查不到（如计划已删除）时对应名字为 None，不影响统计主流程
pub fn lookup_names(
    conn: &Connection,
    aggregator_id: i64,
    plan_id: Option<i64>,
) -> (Option<String>, Option<String>) {
    let agg = conn
        .query_row(
            "SELECT name FROM aggregators WHERE id=?1",
            params![aggregator_id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let plan = plan_id
        .and_then(|pid| {
            conn.query_row(
                "SELECT name FROM coding_plans WHERE id=?1",
                params![pid],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
        });
    (agg, plan)
}

pub fn get_message(conn: &Connection, id: i64) -> Result<Option<MessageRow>> {
    let sql = format!(
        "SELECT {MSG_COLS} FROM messages m
         LEFT JOIN aggregators a ON a.id = m.aggregator_id
         LEFT JOIN coding_plans p ON p.id = m.plan_id
         WHERE m.id=?1"
    );
    match conn.query_row(&sql, params![id], row_to_raw).optional()? {
        // 引用链回溯另发点查，不能在JOIN语句的行借用内进行
        Some(raw) => to_message_row(conn, raw).map(Some),
        None => Ok(None),
    }
}

/// 分页列出消息（按 id 倒序，不含请求/响应体），返回 (总数, 本页数据)
pub fn list_messages(
    conn: &Connection,
    aggregator_id: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<MessageSummary>)> {
    let (count_sql, list_sql) = match aggregator_id {
        Some(_agg_id) => (
            "SELECT COUNT(*) FROM messages WHERE aggregator_id=?1".to_string(),
            format!(
                "SELECT {MSG_SUMMARY_COLS} FROM messages m
                 LEFT JOIN aggregators a ON a.id = m.aggregator_id
                 LEFT JOIN coding_plans p ON p.id = m.plan_id
                 WHERE m.aggregator_id=?1 ORDER BY m.id DESC LIMIT ?2 OFFSET ?3"
            ),
        ),
        None => (
            "SELECT COUNT(*) FROM messages".to_string(),
            format!(
                "SELECT {MSG_SUMMARY_COLS} FROM messages m
                 LEFT JOIN aggregators a ON a.id = m.aggregator_id
                 LEFT JOIN coding_plans p ON p.id = m.plan_id
                 ORDER BY m.id DESC LIMIT ?2 OFFSET ?3"
            ),
        ),
    };

    let total = match aggregator_id {
        Some(agg_id) => conn.query_row(&count_sql, params![agg_id], |row| row.get::<_, i64>(0))?,
        None => conn.query_row(&count_sql, [], |row| row.get::<_, i64>(0))?,
    };

    let mut stmt = conn.prepare(&list_sql)?;
    let rows = stmt.query_map(params![aggregator_id, limit, offset], row_to_summary)?;
    let items = rows.collect::<Result<Vec<_>>>()?;
    Ok((total, items))
}

/// 清空消息。引用只在同聚合器内建立，所以按聚合器清空删掉的一定是完整的引用闭包，
/// 幸存行不需要改写。store 是近期请求体缓存：须与 DELETE 同处持锁区间一并清理，
/// 否则被删的行还可能被新行选作前缀来源
pub fn clear_messages(
    conn: &Connection,
    aggregator_id: Option<i64>,
    store: &BodyStore,
) -> Result<usize> {
    let n = match aggregator_id {
        Some(id) => conn.execute("DELETE FROM messages WHERE aggregator_id=?1", params![id])?,
        None => conn.execute("DELETE FROM messages", [])?,
    };
    match aggregator_id {
        Some(id) => store.remove_aggregator(id),
        None => store.clear(),
    }
    Ok(n)
}

/// 游标之后是否还有未压缩（明文）行：走 idx_messages_legacy，O(1)
pub fn has_legacy_rows(conn: &Connection, after_id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM messages WHERE id > ?1 AND typeof(request_body)='text' LIMIT 1",
            params![after_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// 未压缩（明文）行数：只数 idx_messages_legacy 的索引项，不碰大表
pub fn legacy_message_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE typeof(request_body)='text'",
        [],
        |r| r.get(0),
    )?)
}

// ---------------------------------------------------------------------------
// 存储：页级统计 / 历史消息回填压缩 / 文件整理
// ---------------------------------------------------------------------------

/// 库文件的页级统计。各项都读文件头或数部分索引，不扫表
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageStats {
    pub page_size: i64,
    pub page_count: i64,
    /// 空闲页数：回填压缩把行改小、或删除消息后留下的页，VACUUM 前不计入有效数据
    pub freelist_count: i64,
    /// 仍为明文（未压缩）的消息行数
    pub legacy_rows: i64,
}

impl StorageStats {
    /// 库文件字节数
    pub fn file_bytes(&self) -> i64 {
        self.page_size * self.page_count
    }

    /// 可回收字节数（空闲页）
    pub fn reclaimable_bytes(&self) -> i64 {
        self.page_size * self.freelist_count
    }
}

pub fn storage_stats(conn: &Connection) -> Result<StorageStats> {
    let pragma = |name: &str| -> Result<i64> {
        Ok(conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0))?)
    };
    Ok(StorageStats {
        page_size: pragma("page_size")?,
        page_count: pragma("page_count")?,
        freelist_count: pragma("freelist_count")?,
        legacy_rows: legacy_message_count(conn)?,
    })
}

/// 一批回填压缩的结果。cursor 按 id 单调推进，重复调用直到 finished
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompressStep {
    /// 本批处理到的最后一行 id（下一批从它之后继续；本批无行则原样返回）
    pub cursor: i64,
    /// 本批处理的行数
    pub rows: usize,
    /// 本批处理前 / 后的请求体+响应体字节数
    pub bytes_in: usize,
    pub bytes_out: usize,
    /// cursor 之后已没有明文行
    pub finished: bool,
}

/// 回填一批历史消息：把请求体仍为明文（TEXT，旧库写入）的行改写为编码存储，
/// 规则与在线落库一致（前缀增量 + zstd），对消息内容无损。
///
/// - 按 id 升序每次最多 max_rows 行、约 max_bytes 字节（至少一行），保证单次持锁
///   时间有界；须在持有数据库锁期间调用。
/// - 整批一个事务：中途出错整体回滚，这些行仍是明文，可续跑；已处理的行不再是
///   TEXT，不会被二次处理（幂等）。
/// - cache 是跨批保留的近期请求体缓存（任务私有）；来源行写入前仍逐个确认在库中，
///   批间被清空的消息不会被引用。
/// - level 为压缩级别：回填不在请求路径上，用高于在线的级别换更小体积
pub fn compress_batch(
    conn: &Connection,
    cursor: i64,
    max_rows: usize,
    max_bytes: usize,
    level: i32,
    cache: &BodyStore,
) -> Result<CompressStep> {
    let tx = conn.unchecked_transaction()?;
    let mut step = CompressStep {
        cursor,
        ..Default::default()
    };
    let mut enc = body::Encoder::new(level);
    // 先读完一批再写：UPDATE 会让行离开正在扫描的部分索引，不在扫描途中做
    let mut pending: Vec<(i64, Vec<u8>, Option<i64>, Option<i64>, Vec<u8>)> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, aggregator_id, octet_length(request_body), octet_length(response_body),
                    request_body, response_body
             FROM messages WHERE id > ?1 AND typeof(request_body)='text'
             ORDER BY id LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![cursor, max_rows.max(1) as i64])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let aggregator_id: i64 = row.get(1)?;
            let row_bytes = (row.get::<_, i64>(2)? + row.get::<_, i64>(3)?) as usize;
            // 至少处理一行，否则游标推不动、任务无法前进
            if !pending.is_empty() && step.bytes_in + row_bytes > max_bytes {
                break;
            }
            let req = StoredBody::from_row(row, 4)?;
            let resp = StoredBody::from_row(row, 5)?;
            step.bytes_in += req.bytes.len() + resp.bytes.len();

            let body = req.bytes.as_slice();
            let mut depth = 0;
            let (req_blob, ref_id, prefix_len) = match cache.plan_delta(aggregator_id, body) {
                Some(d) if ref_exists(&tx, d.ref_id, aggregator_id)? => {
                    depth = d.depth;
                    (
                        enc.encode(&body[d.prefix_len..]),
                        Some(d.ref_id),
                        Some(d.prefix_len as i64),
                    )
                }
                _ => (enc.encode(body), None, None),
            };
            let resp_blob = enc.encode(&resp.bytes);
            step.bytes_out += req_blob.len() + resp_blob.len();
            pending.push((id, req_blob, ref_id, prefix_len, resp_blob));
            cache.push(aggregator_id, id, Arc::from(body), depth);
            step.cursor = id;
            step.rows += 1;
        }
    }
    for (id, req_blob, ref_id, prefix_len, resp_blob) in &pending {
        tx.execute(
            "UPDATE messages SET request_body=?1, response_body=?2, req_ref_id=?3, req_prefix_len=?4
             WHERE id=?5 AND typeof(request_body)='text'",
            params![req_blob, resp_blob, ref_id, prefix_len, id],
        )?;
    }
    tx.commit()?;
    step.finished = !has_legacy_rows(conn, step.cursor)?;
    Ok(step)
}

/// 整理库文件（VACUUM）：把空闲页还给文件系统。全库重写，耗时与库体积成正比，
/// 期间独占连接，调用方须保证此时没有代理在用库。
///
/// cancel 置位则中断：VACUUM 是原子的，中断后库保持原样，返回 Ok(false)。
/// check_every 是进度回调的 VM 指令间隔（仅测试调小）。
///
/// temp_store 必须临时改为 FILE：本连接常态是 MEMORY，而 VACUUM 的临时库（整库副本）
/// 同受 temp_store 支配，不改会把整个新库放进内存。任何出口都恢复原设置
pub fn vacuum(conn: &Connection, cancel: Arc<AtomicBool>, check_every: i32) -> Result<bool> {
    conn.execute_batch("PRAGMA temp_store=FILE")?;
    let flag = Arc::clone(&cancel);
    conn.progress_handler(check_every, Some(move || flag.load(Ordering::Relaxed)));
    let result = conn.execute_batch("VACUUM");
    // 先摘回调：收 WAL 时取消标志可能仍置位
    conn.progress_handler(0, None::<fn() -> bool>);
    // 无论成败都恢复 temp_store、收一次 WAL（中断的 VACUUM 回滚后 WAL 仍可能很大）。
    // 这两步失败不改变 VACUUM 的成败结论，不向上抛
    let _ = conn.execute_batch("PRAGMA temp_store=MEMORY");
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    match result {
        Ok(()) => Ok(true),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::OperationInterrupted =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        init_db(":memory:").unwrap()
    }

    #[test]
    fn plan_crud() {
        let conn = mem();
        let p = create_plan(
            &conn,
            "p1",
            "https://a.example",
            "tok1",
            "r",
            &["glm-4.7".into(), "claude-sonnet-5".into()],
        )
        .unwrap();
        assert!(p.enabled);
        assert_eq!(
            p.models,
            vec!["glm-4.7".to_string(), "claude-sonnet-5".to_string()]
        );
        let got = get_plan(&conn, p.id).unwrap().unwrap();
        assert_eq!(got.name, "p1");
        assert_eq!(got.models, p.models);
        // models 写入时 trim + 去空
        update_plan(
            &conn,
            p.id,
            "p1x",
            "https://b.example",
            "tok2",
            "r2",
            false,
            &[" glm-4.7 ".into(), String::new(), ",,".into()],
        )
        .unwrap();
        let got = get_plan(&conn, p.id).unwrap().unwrap();
        assert_eq!(got.name, "p1x");
        assert!(!got.enabled);
        assert_eq!(got.models, vec!["glm-4.7".to_string()]);
        assert_eq!(delete_plan(&conn, p.id).unwrap(), 1);
        assert!(get_plan(&conn, p.id).unwrap().is_none());
    }

    #[test]
    fn plan_full_preserves_disabled() {
        let conn = mem();
        // create_plan 固定 enabled=1，导入路径须能落库 enabled=false
        let p = create_plan_full(
            &conn,
            "p1",
            "https://a.example",
            "tok1",
            "r",
            &["glm-4.7".into()],
            false,
        )
        .unwrap();
        assert!(!p.enabled);
        let got = get_plan(&conn, p.id).unwrap().unwrap();
        assert!(!got.enabled);
        assert_eq!(got.models, vec!["glm-4.7".to_string()]);
        // enabled=true 路径同样往返
        let p2 = create_plan_full(&conn, "p2", "https://b.example", "tok2", "", &[], true)
            .unwrap();
        assert!(get_plan(&conn, p2.id).unwrap().unwrap().enabled);
    }

    #[test]
    fn clear_config_keeps_messages_and_settings() {
        let conn = mem();
        let p = create_plan(&conn, "p1", "https://a", "t1", "", &[]).unwrap();
        let agg =
            create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        set_aggregator_plans(&conn, agg.id, &[p.id]).unwrap();
        set_current_plan(&conn, agg.id, p.id).unwrap();
        set_setting(&conn, "trip_started_at", "2026-10-03 00:00:00").unwrap();
        insert_message(
            &conn,
            &NewMessage {
                aggregator_id: agg.id,
                plan_id: Some(p.id),
                method: "POST".into(),
                path: "/v1/messages".into(),
                status: 200,
                request_body: "{}".into(),
                response_body: "{}".into(),
                model: "glm-4.7".into(),
                prompt_tokens: 1,
                completion_tokens: 2,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                total_tokens: 3,
                duration_ms: 10,
            },
            &BodyStore::new(),
        )
        .unwrap();

        clear_config(&conn).unwrap();

        assert!(list_plans(&conn).unwrap().is_empty());
        assert!(list_aggregators(&conn).unwrap().is_empty());
        // messages 与 settings 不受影响（replace 导入的语义防线）
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            get_setting(&conn, "trip_started_at").unwrap(),
            Some("2026-10-03 00:00:00".to_string())
        );
    }

    #[test]
    fn bindings_and_reset() {
        let conn = mem();
        let p1 = create_plan(&conn, "p1", "https://a", "t1", "", &["glm-4.7".into()]).unwrap();
        let p2 = create_plan(&conn, "p2", "https://b", "t2", "", &[]).unwrap();
        let agg = create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
            .unwrap();
        set_aggregator_plans(&conn, agg.id, &[p1.id, p2.id]).unwrap();

        let bs = list_bindings(&conn, agg.id).unwrap();
        assert_eq!(bs.len(), 2);
        assert_eq!(bs[0].plan.id, p1.id);
        assert_eq!(bs[0].plan.models, vec!["glm-4.7".to_string()]);
        assert!(bs[1].plan.models.is_empty());
        assert_eq!(bs[0].used_tokens, 0);

        add_binding_usage(&conn, bs[0].binding_id, 40).unwrap();
        add_binding_usage(&conn, bs[0].binding_id, 10).unwrap();
        let bs = list_bindings(&conn, agg.id).unwrap();
        assert_eq!(bs[0].used_tokens, 50);

        // 重排后 used_tokens 保留
        set_aggregator_plans(&conn, agg.id, &[p2.id, p1.id]).unwrap();
        let bs = list_bindings(&conn, agg.id).unwrap();
        assert_eq!(bs[0].plan.id, p2.id);
        assert_eq!(bs[1].used_tokens, 50);

        reset_binding_usage(&conn, agg.id).unwrap();
        let bs = list_bindings(&conn, agg.id).unwrap();
        assert!(bs.iter().all(|b| b.used_tokens == 0));
    }

    #[test]
    fn messages_paging_and_stats() {
        let conn = mem();
        let p1 = create_plan(&conn, "p1", "https://a", "t1", "", &[]).unwrap();
        let agg = create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
            .unwrap();
        for i in 0..5 {
            insert_message(
                &conn,
                &NewMessage {
                    aggregator_id: agg.id,
                    plan_id: Some(p1.id),
                    method: "POST".into(),
                    path: format!("/v1/messages/{i}"),
                    status: 200,
                    request_body: "req".into(),
                    response_body: "resp".into(),
                    model: "claude-sonnet-5".into(),
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    cache_read_tokens: 40,
                    cache_creation_tokens: 20,
                    total_tokens: 75,
                    duration_ms: 3,
                },
                &BodyStore::new(),
            )
            .unwrap();
        }
        let (total, items) = list_messages(&conn, Some(agg.id), 3, 0).unwrap();
        assert_eq!(total, 5);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].path, "/v1/messages/4"); // 倒序
        assert_eq!(items[0].aggregator_name.as_deref(), Some("g1"));
        assert_eq!(items[0].plan_name.as_deref(), Some("p1"));

        let s = aggregator_stats(&conn, agg.id).unwrap();
        assert_eq!(s.requests, 5);
        assert_eq!(s.total_tokens, 375);
        assert_eq!(s.cache_read_tokens, 200);
        assert_eq!(s.cache_creation_tokens, 100);
        let s = plan_stats(&conn, p1.id).unwrap();
        assert_eq!(s.total_tokens, 375);
        let s = global_stats(&conn).unwrap();
        assert_eq!(s.total_tokens, 375);

        assert_eq!(clear_messages(&conn, Some(agg.id), &BodyStore::new()).unwrap(), 5);
        assert_eq!(global_stats(&conn).unwrap().requests, 0);
    }

    /// 索引定义演进时 ensure_index 应 DROP 重建为最新定义；定义一致时幂等
    #[test]
    fn ensure_index_rebuilds_on_definition_change() {
        let conn = mem();
        let (name, ddl) = STAT_INDEX_DDL
            .iter()
            .find(|(n, _)| *n == "idx_messages_stat_day")
            .unwrap();
        // 模拟库里残留旧版定义（缺普通 created_at 列）
        conn.execute("DROP INDEX idx_messages_stat_day", [])
            .unwrap();
        conn.execute(
            "CREATE INDEX idx_messages_stat_day ON messages(aggregator_id, \
             substr(created_at,1,10), substr(created_at,12,2), total_tokens, \
             prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens)",
            [],
        )
        .unwrap();
        ensure_index(&conn, name, ddl).unwrap();
        let cur: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='idx_messages_stat_day'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(norm_sql(&cur), norm_sql(ddl));
        // 定义已一致，再跑一次应直接返回（重建路径不会触发 DROP）
        ensure_index(&conn, name, ddl).unwrap();
    }

    /// 回归防线：旧版库（messages 无 model/cache 列、coding_plans 无 models、
    /// aggregators 无 current_plan_id）走完整 init_schema 必须先补列再建索引，
    /// 否则 ensure_index 引用不存在列报 "no such column"——安装包在装过旧版的
    /// 机器上打开即闪退的根因
    #[test]
    fn init_schema_on_legacy_db_migrates_before_indexing() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT, aggregator_id INTEGER NOT NULL,
                plan_id INTEGER, method TEXT NOT NULL DEFAULT '', path TEXT NOT NULL DEFAULT '',
                status INTEGER NOT NULL DEFAULT 0, request_body TEXT NOT NULL DEFAULT '',
                response_body TEXT NOT NULL DEFAULT '', prompt_tokens INTEGER NOT NULL DEFAULT 0,
                completion_tokens INTEGER NOT NULL DEFAULT 0, total_tokens INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL);
            CREATE TABLE coding_plans (
                id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, base_url TEXT NOT NULL,
                auth_token TEXT NOT NULL, remark TEXT NOT NULL DEFAULT '',
                enabled INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL);
            CREATE TABLE aggregators (
                id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, port INTEGER NOT NULL,
                auth_token TEXT NOT NULL, strategy TEXT NOT NULL DEFAULT 'threshold_rotation',
                token_threshold INTEGER NOT NULL DEFAULT 1000000, created_at TEXT NOT NULL);
            INSERT INTO messages (aggregator_id, request_body, response_body, created_at)
                VALUES (1, 'legacy 请求体', 'legacy 响应体', '2026-01-01 00:00:00');",
        )
        .unwrap();

        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap(); // 幂等

        let cols = |table: &str| -> Vec<String> {
            let mut v = Vec::new();
            let mut stmt = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap();
            for r in stmt.query_map([], |row| row.get::<_, String>(0)).unwrap() {
                v.push(r.unwrap());
            }
            v
        };
        let msg = cols("messages");
        for c in [
            "model",
            "cache_read_tokens",
            "cache_creation_tokens",
            "req_ref_id",
            "req_prefix_len",
        ] {
            assert!(msg.iter().any(|x| x == c), "迁移后 messages 应有列 {c}");
        }
        // 引用两列追加在末尾（与新建表的列序一致，MSG_COLS 按此取列）
        assert_eq!(msg.last().unwrap(), "req_prefix_len");
        for (name, _) in STAT_INDEX_DDL {
            let exists = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
                    params![name],
                    |_| Ok(()),
                )
                .optional()
                .unwrap()
                .is_some();
            assert!(exists, "统计索引 {name} 应已建成");
        }
        // 未压缩行部分索引已建，既有明文行被收录且照常可读
        let plan = plan_of(&conn, "SELECT COUNT(*) FROM messages WHERE typeof(request_body)='text'");
        assert!(plan.contains("idx_messages_legacy"), "{plan}");
        assert_eq!(legacy_message_count(&conn).unwrap(), 1);
        let m = get_message(&conn, 1).unwrap().unwrap();
        assert_eq!(m.request_body, "legacy 请求体");
        assert_eq!(m.response_body, "legacy 响应体");
    }

    /// 回归防线：统计查询必须命中覆盖索引（idx_messages_stat_*），
    /// 防止表达式索引书写与查询失配后悄然退化为遍历溢出页链的全表扫描
    #[test]
    fn stats_queries_hit_covering_indexes() {
        let conn = mem();
        let p1 = create_plan(&conn, "p1", "https://a", "t1", "", &[]).unwrap();
        let agg = create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
            .unwrap();
        // 空表时 planner 可能选全表扫描，先插入少量行
        for i in 0..5 {
            insert_message(
                &conn,
                &NewMessage {
                    aggregator_id: agg.id,
                    plan_id: Some(p1.id),
                    method: "POST".into(),
                    path: format!("/v1/messages/{i}"),
                    status: 200,
                    request_body: "req".into(),
                    response_body: "resp".into(),
                    model: if i % 2 == 0 { "glm-4.7" } else { "claude-sonnet-5" }.into(),
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    cache_read_tokens: 40,
                    cache_creation_tokens: 20,
                    total_tokens: 75,
                    duration_ms: 3,
                },
                &BodyStore::new(),
            )
            .unwrap();
        }

        // 统计查询形状全集（与 bucket_query 实际生成的 SQL 一致，含 ORDER BY 与聚合器过滤变体）：
        // 累计 / 按聚合器 / 按计划 / 小计时间范围 / 按天 / 按小时 / 按模型
        let cases = [
            STATS_SELECT.to_string(),
            format!("{STATS_SELECT} WHERE aggregator_id=?1"),
            format!("{STATS_SELECT} WHERE plan_id=?1"),
            format!("{STATS_SELECT} WHERE created_at>=?1"),
            "SELECT substr(created_at,1,10) AS k, COALESCE(SUM(total_tokens),0), COUNT(*) \
             FROM messages WHERE substr(created_at,1,10)>=?1 GROUP BY k ORDER BY k"
                .to_string(),
            "SELECT substr(created_at,1,10) AS k, COALESCE(SUM(total_tokens),0), COUNT(*) \
             FROM messages WHERE aggregator_id=?1 AND substr(created_at,1,10)>=?2 \
             GROUP BY k ORDER BY k"
                .to_string(),
            "SELECT substr(created_at,12,2) AS k, COALESCE(SUM(total_tokens),0), COUNT(*) \
             FROM messages WHERE substr(created_at,1,10)=?1 GROUP BY k \
             ORDER BY CAST(k AS INTEGER)"
                .to_string(),
            "SELECT substr(created_at,12,2) AS k, COALESCE(SUM(total_tokens),0), COUNT(*) \
             FROM messages WHERE aggregator_id=?1 AND substr(created_at,1,10)=?2 \
             GROUP BY k ORDER BY CAST(k AS INTEGER)"
                .to_string(),
            "SELECT model AS k, COALESCE(SUM(total_tokens),0), COUNT(*) \
             FROM messages GROUP BY k ORDER BY COALESCE(SUM(total_tokens),0) DESC, k"
                .to_string(),
            "SELECT model AS k, COALESCE(SUM(total_tokens),0), COUNT(*) \
             FROM messages WHERE aggregator_id=?1 GROUP BY k \
             ORDER BY COALESCE(SUM(total_tokens),0) DESC, k"
                .to_string(),
            "SELECT model AS k, COALESCE(SUM(total_tokens),0), COUNT(*) \
             FROM messages WHERE aggregator_id=?1 AND substr(created_at,1,10)>=?2 GROUP BY k \
             ORDER BY COALESCE(SUM(total_tokens),0) DESC, k"
                .to_string(),
        ];
        for sql in cases {
            // 占位符须绑定哑值才能 EXPLAIN（取一个足够早的日期下界，不影响计划选择）
            let dummies: Vec<&str> = vec!["1970-01-01"; sql.matches('?').count()];
            let plans: Vec<String> = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(rusqlite::params_from_iter(dummies), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert!(
                plans
                    .iter()
                    .any(|p| p.contains("USING COVERING INDEX idx_messages_stat")),
                "统计查询应命中覆盖索引: {sql} -> {plans:?}"
            );
        }
    }

    #[test]
    fn trip_meter() {
        let conn = mem();
        let agg = create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
            .unwrap();
        // 插入一条 total_tokens=total 的消息；created 为 Some 时改写 created_at 便于构造时间线
        let insert_at = |total: i64, created: Option<&str>| {
            insert_message(
                &conn,
                &NewMessage {
                    aggregator_id: agg.id,
                    plan_id: None,
                    method: "POST".into(),
                    path: "/v1/messages".into(),
                    status: 200,
                    request_body: "".into(),
                    response_body: "".into(),
                    model: "glm-4.7".into(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    total_tokens: total,
                    duration_ms: 1,
                },
                &BodyStore::new(),
            )
            .unwrap();
            if let Some(c) = created {
                conn.execute(
                    "UPDATE messages SET created_at=?1 WHERE id=(SELECT MAX(id) FROM messages)",
                    params![c],
                )
                .unwrap();
            }
        };

        // settings 读写 + 幂等覆写
        assert_eq!(get_setting(&conn, "k").unwrap(), None);
        set_setting(&conn, "k", "v1").unwrap();
        set_setting(&conn, "k", "v2").unwrap();
        assert_eq!(get_setting(&conn, "k").unwrap().as_deref(), Some("v2"));

        // 未重置：小计与累计一致，计入全部历史
        insert_at(100, Some("2001-01-01 00:00:00"));
        insert_at(50, Some("2020-06-01 12:00:00"));
        let t = trip_stats(&conn).unwrap();
        assert!(t.started_at.is_none());
        assert_eq!(t.stats.total_tokens, 150);
        assert_eq!(t.stats.requests, 2);

        // 重置：起点为当前时间；两条旧时间消息均不计入
        let t = reset_trip(&conn).unwrap();
        assert!(t.started_at.is_some());
        assert_eq!(t.stats.total_tokens, 0);
        assert_eq!(t.stats.requests, 0);

        // 起点之前的不计入，之后（落库即当前时间）的计入
        insert_at(30, Some("2001-01-01 00:00:00"));
        insert_at(70, None);
        let t = trip_stats(&conn).unwrap();
        assert_eq!(t.stats.total_tokens, 70);
        assert_eq!(t.stats.requests, 1);

        // 累计统计不受重置影响
        let g = global_stats(&conn).unwrap();
        assert_eq!(g.total_tokens, 250);
        assert_eq!(g.requests, 4);
    }

    #[test]
    fn trip_pause() {
        let conn = mem();
        let agg = create_aggregator(&conn, "g1", 8301, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
            .unwrap();
        let insert_at = |total: i64, created: &str| {
            insert_message(
                &conn,
                &NewMessage {
                    aggregator_id: agg.id,
                    plan_id: None,
                    method: "POST".into(),
                    path: "/v1/messages".into(),
                    status: 200,
                    request_body: "".into(),
                    response_body: "".into(),
                    model: "glm-4.7".into(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    total_tokens: total,
                    duration_ms: 1,
                },
                &BodyStore::new(),
            )
            .unwrap();
            conn.execute(
                "UPDATE messages SET created_at=?1 WHERE id=(SELECT MAX(id) FROM messages)",
                params![created],
            )
            .unwrap();
        };

        // 固定 2020 年时间线（手工写 settings，避免真实时钟秒级竞争）：
        //   00:30 计数中 A=100；01:00 暂停（P=10）；02:00 暂停期间 B=50；
        //   03:00 恢复（R=20）；04:00 恢复后 C=70
        set_setting(&conn, SETTING_TRIP_STARTED_AT, "2020-01-01 00:00:00").unwrap();
        insert_at(100, "2020-01-01 00:30:00");
        let t = trip_stats(&conn).unwrap();
        assert!(!t.paused);
        assert_eq!(t.stats.total_tokens, 100);

        // 暂停：冻结在 01:00 之前，之后的消息不计入
        set_setting(&conn, SETTING_TRIP_PAUSED_AT, "2020-01-01 01:00:00").unwrap();
        insert_at(10, "2020-01-01 01:00:00"); // 暂停瞬间（含）
        insert_at(50, "2020-01-01 02:00:00"); // 暂停期间
        let t = trip_stats(&conn).unwrap();
        assert!(t.paused);
        assert_eq!(t.stats.total_tokens, 100);
        assert_eq!(t.stats.requests, 1);

        // 继续：闭合区间 [01:00, 03:00)，区间内消息不补计；恢复瞬间及之后的计入
        set_setting(
            &conn,
            SETTING_TRIP_PAUSES,
            "[[\"2020-01-01 01:00:00\",\"2020-01-01 03:00:00\"]]",
        )
        .unwrap();
        delete_setting(&conn, SETTING_TRIP_PAUSED_AT).unwrap();
        insert_at(20, "2020-01-01 03:00:00"); // 恢复瞬间（含）
        insert_at(70, "2020-01-01 04:00:00");
        let t = trip_stats(&conn).unwrap();
        assert!(!t.paused);
        assert_eq!(t.stats.total_tokens, 190);
        assert_eq!(t.stats.requests, 3);

        // 累计统计不受暂停影响（双轨）
        let g = global_stats(&conn).unwrap();
        assert_eq!(g.total_tokens, 250);
        assert_eq!(g.requests, 5);

        // toggle 状态机（真实时钟，只断言状态与区间记录，不断言 token 边界）
        let t = toggle_trip_pause(&conn).unwrap();
        assert!(t.paused);
        let t = toggle_trip_pause(&conn).unwrap();
        assert!(!t.paused);
        let raw = get_setting(&conn, SETTING_TRIP_PAUSES).unwrap().unwrap();
        let pauses: Vec<(String, String)> = serde_json::from_str(&raw).unwrap();
        assert_eq!(pauses.len(), 2, "手工区间 + toggle 闭合区间");

        // 暂停中重置：清零并回到计数中，暂停状态一并清除
        toggle_trip_pause(&conn).unwrap();
        let t = reset_trip(&conn).unwrap();
        assert!(!t.paused);
        assert_eq!(t.stats.total_tokens, 0);
        assert!(get_setting(&conn, SETTING_TRIP_PAUSED_AT)
            .unwrap()
            .is_none());
        assert!(get_setting(&conn, SETTING_TRIP_PAUSES).unwrap().is_none());
    }

    #[test]
    fn delete_cascades_bindings() {
        let conn = mem();
        let p1 = create_plan(&conn, "p1", "https://a", "t1", "", &[]).unwrap();
        let p2 = create_plan(&conn, "p2", "https://b", "t2", "", &[]).unwrap();
        let agg1 = create_aggregator(&conn, "g1", 8300, "cpm-1", 100, STRATEGY_THRESHOLD_ROTATION)
            .unwrap();
        let agg2 = create_aggregator(&conn, "g2", 8301, "cpm-2", 100, STRATEGY_THRESHOLD_ROTATION)
            .unwrap();

        // 删除计划：其在所有聚合器下的绑定一并解除
        set_aggregator_plans(&conn, agg1.id, &[p1.id, p2.id]).unwrap();
        set_aggregator_plans(&conn, agg2.id, &[p1.id]).unwrap();
        assert_eq!(delete_plan(&conn, p1.id).unwrap(), 1);
        assert_eq!(
            list_bindings(&conn, agg1.id).unwrap().len(),
            1,
            "agg1 应只剩 p2"
        );
        assert!(
            list_bindings(&conn, agg2.id).unwrap().is_empty(),
            "agg2 绑定应全部解除"
        );

        // 删除聚合器：其下绑定解除，其他聚合器不受影响
        set_aggregator_plans(&conn, agg1.id, &[p2.id]).unwrap();
        assert_eq!(delete_aggregator(&conn, agg2.id).unwrap(), 1);
        assert!(list_bindings(&conn, agg2.id).unwrap().is_empty());
        assert_eq!(
            list_bindings(&conn, agg1.id).unwrap().len(),
            1,
            "agg1 的绑定不应受影响"
        );
    }

    #[test]
    fn grouped_stats_and_migration() {
        let conn = mem();
        let p1 = create_plan(&conn, "p1", "https://a", "t1", "", &[]).unwrap();
        let agg = create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
            .unwrap();
        // NewMessage 不含 created_at（落库固定用 now_str），插入后用 SQL 改写时间
        let mk = |model: &str, created: &str, prompt: i64, completion: i64| {
            (
                NewMessage {
                    aggregator_id: agg.id,
                    plan_id: Some(p1.id),
                    method: "POST".into(),
                    path: "/v1/messages".into(),
                    status: 200,
                    request_body: "req".into(),
                    response_body: "resp".into(),
                    model: model.into(),
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    total_tokens: prompt + completion,
                    duration_ms: 1,
                },
                created.to_string(),
            )
        };
        let insert_at = |m: NewMessage, created: String| {
            insert_message(&conn, &m, &BodyStore::new()).unwrap();
            conn.execute(
                "UPDATE messages SET created_at=?1 WHERE id=(SELECT MAX(id) FROM messages)",
                params![created],
            )
            .unwrap();
        };
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let (m, c) = mk("glm-4.7", &format!("{today} 09:30:00"), 100, 50);
        insert_at(m, c);
        let (m, c) = mk("glm-4.7", &format!("{today} 21:05:00"), 200, 100);
        insert_at(m, c);
        let (m, c) = mk("claude-sonnet-5", &format!("{today} 14:00:00"), 10, 5);
        insert_at(m, c);
        let (m, c) = mk("glm-4.7", "2001-01-01 08:00:00", 999, 1); // 超出时间范围
        insert_at(m, c);

        // 按天：cutoff 之外的不计入
        let days = stats_daily(&conn, None, 1).unwrap();
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].key, today);
        assert_eq!(days[0].total_tokens, 100 + 50 + 200 + 100 + 10 + 5);
        assert_eq!(days[0].requests, 3);

        // 按小时：09 点与 21 点
        let hours = stats_hourly(&conn, None, &today).unwrap();
        assert_eq!(hours.len(), 3);
        assert_eq!(hours[0].key, "09");
        assert_eq!(hours[0].total_tokens, 150);
        assert_eq!(hours[2].key, "21");
        assert_eq!(hours[2].total_tokens, 300);

        // 按模型：按总量倒序，glm-4.7（含 cutoff 外的 1000）在前
        let models = stats_by_model(&conn, None, None).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].key, "glm-4.7");
        assert_eq!(models[0].total_tokens, 150 + 300 + 1000);
        assert_eq!(models[1].key, "claude-sonnet-5");
        assert_eq!(models[1].requests, 1);

        // 按模型 + 近 1 天：cutoff 外的 1000 不计入
        let models = stats_by_model(&conn, None, Some(1)).unwrap();
        assert_eq!(models[0].key, "glm-4.7");
        assert_eq!(models[0].total_tokens, 150 + 300);

        // 聚合器过滤（其他聚合器 id 应为空）
        assert!(stats_daily(&conn, Some(agg.id + 100), 1)
            .unwrap()
            .is_empty());

        // 日期按字面量绑定：含 SQL 特殊字符的 date 不会被执行为语法
        let evil = format!("{today}' OR '1'='1");
        assert!(stats_hourly(&conn, None, &evil).unwrap().is_empty());

        // 迁移：旧库（无 model 列）打开后自动补列
        let old = Connection::open_in_memory().unwrap();
        old.execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT, aggregator_id INTEGER NOT NULL,
                plan_id INTEGER, method TEXT NOT NULL DEFAULT '', path TEXT NOT NULL DEFAULT '',
                status INTEGER NOT NULL DEFAULT 0, request_body TEXT NOT NULL DEFAULT '',
                response_body TEXT NOT NULL DEFAULT '', prompt_tokens INTEGER NOT NULL DEFAULT 0,
                completion_tokens INTEGER NOT NULL DEFAULT 0, total_tokens INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL);
            CREATE TABLE coding_plans (
                id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, base_url TEXT NOT NULL,
                auth_token TEXT NOT NULL, remark TEXT NOT NULL DEFAULT '',
                enabled INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL);
            CREATE TABLE aggregators (
                id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, port INTEGER NOT NULL,
                auth_token TEXT NOT NULL, strategy TEXT NOT NULL DEFAULT 'threshold_rotation',
                token_threshold INTEGER NOT NULL DEFAULT 1000000, created_at TEXT NOT NULL);",
        )
        .unwrap();
        migrate(&old).unwrap();
        migrate(&old).unwrap(); // 幂等
        let table_cols = |table: &str| -> Vec<String> {
            let mut cols: Vec<String> = Vec::new();
            let mut stmt = old
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap();
            let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
            for r in rows {
                cols.push(r.unwrap());
            }
            cols
        };
        for c in ["model", "cache_read_tokens", "cache_creation_tokens"] {
            assert!(
                table_cols("messages").iter().any(|x| x == c),
                "迁移后 messages 应有列 {c}"
            );
        }
        assert!(
            table_cols("coding_plans").iter().any(|x| x == "models"),
            "迁移后 coding_plans 应有 models 列"
        );
        assert!(
            table_cols("aggregators")
                .iter()
                .any(|x| x == "current_plan_id"),
            "迁移后 aggregators 应有 current_plan_id 列"
        );
        old.execute(
            "INSERT INTO messages (aggregator_id, created_at) VALUES (1, '2026-08-17 10:00:00')",
            [],
        )
        .unwrap();
        let buckets = stats_by_model(&old, None, None).unwrap();
        assert_eq!(buckets[0].key, ""); // 旧行 model 为空串
    }

    #[test]
    fn strategy_and_current_plan() {
        assert!(is_known_strategy(STRATEGY_THRESHOLD_ROTATION));
        assert!(is_known_strategy(STRATEGY_MODEL_MATCH));
        assert!(!is_known_strategy("model-match"));
        assert!(!is_known_strategy(""));

        let conn = mem();
        let p1 = create_plan(&conn, "p1", "https://a", "t1", "", &[]).unwrap();
        let p2 = create_plan(&conn, "p2", "https://b", "t2", "", &[]).unwrap();
        let agg = create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_MODEL_MATCH).unwrap();
        assert_eq!(agg.strategy, STRATEGY_MODEL_MATCH);
        assert!(agg.current_plan_id.is_none(), "新建聚合器未设置当前计划");

        // 设置当前计划；db 层 update_aggregator 不触碰 current_plan_id
        // （显式切换策略时是否清除由命令层决定）
        assert!(set_current_plan(&conn, agg.id, p1.id).unwrap());
        let got = get_aggregator(&conn, agg.id).unwrap().unwrap();
        assert_eq!(got.current_plan_id, Some(p1.id));
        update_aggregator(&conn, agg.id, "g1x", 8301, 200, STRATEGY_THRESHOLD_ROTATION).unwrap();
        let got = get_aggregator(&conn, agg.id).unwrap().unwrap();
        assert_eq!(got.name, "g1x");
        assert_eq!(got.strategy, STRATEGY_THRESHOLD_ROTATION);
        assert_eq!(
            got.current_plan_id,
            Some(p1.id),
            "改策略/阈值不应清掉当前计划"
        );

        // 覆盖切换；清除；聚合器不存在返回 false
        assert!(set_current_plan(&conn, agg.id, p2.id).unwrap());
        let got = get_aggregator(&conn, agg.id).unwrap().unwrap();
        assert_eq!(got.current_plan_id, Some(p2.id));
        assert!(!set_current_plan(&conn, agg.id + 100, p1.id).unwrap());
        assert!(!clear_current_plan(&conn, agg.id + 100).unwrap());
        assert!(clear_current_plan(&conn, agg.id).unwrap());
        let got = get_aggregator(&conn, agg.id).unwrap().unwrap();
        assert_eq!(got.current_plan_id, None, "清除后恢复自动轮转");
        let _ = p2;
    }

    // -----------------------------------------------------------------------
    // 消息体编码存储：写入 / 读取 / 清空 / 回填
    // -----------------------------------------------------------------------

    /// EXPLAIN QUERY PLAN 的明细行
    fn plan_of(conn: &Connection, sql: &str) -> String {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
            .join(" | ")
    }

    fn new_msg(aggregator_id: i64, req: &str, resp: &str) -> NewMessage {
        NewMessage {
            aggregator_id,
            plan_id: None,
            method: "POST".into(),
            path: "/v1/messages".into(),
            status: 200,
            request_body: req.into(),
            response_body: resp.into(),
            model: "glm-4.7".into(),
            prompt_tokens: 1,
            completion_tokens: 1,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            total_tokens: 2,
            duration_ms: 1,
        }
    }

    /// 旧库形态的明文行：绕过 insert_message，以 TEXT 原样落库
    fn insert_legacy(conn: &Connection, aggregator_id: i64, req: &str, resp: &str) -> i64 {
        conn.execute(
            "INSERT INTO messages (aggregator_id, method, path, request_body, response_body,
                                   model, total_tokens, duration_ms, created_at)
             VALUES (?1,'POST','/v1/messages',?2,?3,'glm-4.7',1,1,'2026-01-01 00:00:00')",
            params![aggregator_id, req, resp],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// 可压缩的伪正文（字母循环），seed 决定相位：不同 seed 的正文首字节即不同
    fn filler(len: usize, seed: u8) -> String {
        (0..len)
            .map(|i| (b'a' + ((i as u8).wrapping_mul(7).wrapping_add(seed)) % 26) as char)
            .collect()
    }

    /// 一行的请求体引用 (req_ref_id, req_prefix_len)
    fn req_ref(conn: &Connection, id: i64) -> (Option<i64>, Option<i64>) {
        conn.query_row(
            "SELECT req_ref_id, req_prefix_len FROM messages WHERE id=?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    /// 全部消息的请求体+响应体落库字节数
    fn stored_bytes(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COALESCE(SUM(octet_length(request_body) + octet_length(response_body)),0)
             FROM messages",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// 断言全部行的请求体引用都指向同一聚合器的更早行（引用闭包不变量）
    fn assert_refs_closed(conn: &Connection) -> usize {
        let mut stmt = conn
            .prepare(
                "SELECT m.id, m.req_ref_id FROM messages m
                 LEFT JOIN messages p ON p.id = m.req_ref_id
                 WHERE m.req_ref_id IS NOT NULL
                   AND (p.id IS NULL OR p.aggregator_id <> m.aggregator_id OR p.id >= m.id)",
            )
            .unwrap();
        let broken = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(broken.is_empty(), "这些行的引用越出了同聚合器的更早行：{broken:?}");
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE req_ref_id IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn insert_and_get_message_roundtrip_blob_vs_legacy() {
        let conn = mem();
        let agg =
            create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        let store = BodyStore::new();
        let long = filler(100_000, 0);
        let bodies = ["req", "", "{}", long.as_str()];
        let ids: Vec<i64> = bodies
            .iter()
            .map(|req| insert_message(&conn, &new_msg(agg.id, req, "resp"), &store).unwrap().0)
            .collect();

        // 新行一律 BLOB：极短的走原始字节（1 字节标签 + 原文）、空体为空 BLOB、
        // 可压缩的走 zstd
        for (id, req) in ids.iter().zip(bodies) {
            let (kind, len): (String, i64) = conn
                .query_row(
                    "SELECT typeof(request_body), octet_length(request_body) FROM messages WHERE id=?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(kind, "blob");
            if req.is_empty() {
                assert_eq!(len, 0);
            } else if req.len() < 16 {
                assert_eq!(len, req.len() as i64 + 1);
            } else {
                assert!(len * 2 < req.len() as i64, "压缩后应显著变小：{len}");
            }
        }
        for (id, req) in ids.iter().zip(bodies) {
            let m = get_message(&conn, *id).unwrap().unwrap();
            assert_eq!(m.request_body, req);
            assert_eq!(m.response_body, "resp");
            assert_eq!(req_ref(&conn, *id).0, None, "不足 16KB 的体不建增量");
        }

        // 旧库明文行（TEXT）照常读出，且只把明文行算作未压缩
        let legacy = insert_legacy(&conn, agg.id, "legacy 请求体", "legacy 响应体");
        let m = get_message(&conn, legacy).unwrap().unwrap();
        assert_eq!(m.request_body, "legacy 请求体");
        assert_eq!(m.response_body, "legacy 响应体");
        assert_eq!(legacy_message_count(&conn).unwrap(), 1);
        assert!(has_legacy_rows(&conn, 0).unwrap());
        assert!(!has_legacy_rows(&conn, legacy).unwrap());
        assert!(get_message(&conn, legacy + 100).unwrap().is_none());
    }

    /// 回归防线：未压缩行的判断/计数/回填扫描必须命中部分索引，
    /// 否则在数十 GB 的库上退化为遍历溢出页链的全表扫描
    #[test]
    fn legacy_queries_use_partial_index() {
        let conn = mem();
        let agg =
            create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        for i in 0..5 {
            insert_legacy(&conn, agg.id, &format!("r{i}"), "p");
        }
        for sql in [
            "SELECT COUNT(*) FROM messages WHERE typeof(request_body)='text'",
            "SELECT 1 FROM messages WHERE id > 3 AND typeof(request_body)='text' LIMIT 1",
            "SELECT id FROM messages WHERE id > 3 AND typeof(request_body)='text' ORDER BY id LIMIT 8",
        ] {
            let plan = plan_of(&conn, sql);
            assert!(plan.contains("idx_messages_legacy"), "{sql} -> {plan}");
        }
    }

    #[test]
    fn get_message_chain_roundtrip() {
        let conn = mem();
        let a1 =
            create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        let a2 =
            create_aggregator(&conn, "g2", 8301, "cpm-y", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        let store = BodyStore::new();
        // 两个聚合器各有一条逐轮增长的会话，行交错落库
        let sys1 = filler(64 * 1024, 0);
        let sys2 = filler(64 * 1024, 1);
        let mut expected = Vec::new();
        for turn in 0..6 {
            for (agg, sys) in [(&a1, &sys1), (&a2, &sys2)] {
                let req = format!("{sys}\n[turn {turn}]{}", "对话内容".repeat(200));
                let id = insert_message(&conn, &new_msg(agg.id, &req, "r"), &store)
                    .unwrap()
                    .0;
                expected.push((id, req));
            }
        }
        for (id, req) in &expected {
            assert_eq!(get_message(&conn, *id).unwrap().unwrap().request_body, *req);
        }
        // 每个聚合器首行整条，其后均为增量
        let roots = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE req_ref_id IS NULL",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(roots, 2);
        assert_eq!(assert_refs_closed(&conn), expected.len() - 2);
    }

    #[test]
    fn get_message_degrades_on_dangling_ref() {
        let conn = mem();
        let agg =
            create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        let store = BodyStore::new();
        let sys = filler(64 * 1024, 0);
        let id = |suffix: &str| {
            insert_message(&conn, &new_msg(agg.id, &format!("{sys} {suffix}"), "r"), &store)
                .unwrap()
                .0
        };
        let root = id("root");
        let child = id("child");
        let grand = id("childgrand");
        // 公共前缀：root/child 到 " " 为止；child/grand 到 " child" 为止
        let prefix = (sys.len() + 1) as i64;
        assert_eq!(req_ref(&conn, child), (Some(root), Some(prefix)));
        assert_eq!(req_ref(&conn, grand), (Some(child), Some(prefix + 5)));

        // 来源行比记录的前缀长度短（标记点名直接来源行；正文不报错，
        // 为「来源行全文 + 自有后缀」）。改回正确长度即恢复
        let great = id("childgrand2");
        assert_eq!(req_ref(&conn, great), (Some(grand), Some(prefix + 10)));
        let full = format!("{sys} childgrand2");
        assert_eq!(get_message(&conn, great).unwrap().unwrap().request_body, full);
        conn.execute(
            "UPDATE messages SET req_prefix_len=999999 WHERE id=?1",
            params![great],
        )
        .unwrap();
        let m = get_message(&conn, great).unwrap().unwrap().request_body;
        assert_eq!(
            m,
            format!(
                "…[引用的请求体 #{grand} 比记录的前缀长度短，前 999999 字节不完整]\n{full}"
            )
        );
        conn.execute(
            "UPDATE messages SET req_prefix_len=?2 WHERE id=?1",
            params![great, prefix + 10],
        )
        .unwrap();
        assert_eq!(get_message(&conn, great).unwrap().unwrap().request_body, full);

        // 根行不在了（正常路径不会发生——按聚合器清空删的是完整闭包；
        // 此处模拟库被异常改动）：不报错，以标记开头 + 仍可还原的后缀
        conn.execute("DELETE FROM messages WHERE id=?1", params![root])
            .unwrap();
        let gone = format!("…[引用的请求体 #{root} 已不存在，前 {prefix} 字节无法恢复]\n");
        assert_eq!(
            get_message(&conn, great).unwrap().unwrap().request_body,
            format!("{gone}childgrand2"),
            "多层同断一个断点：标记只出一次，各层后缀依次拼上"
        );
        assert_eq!(
            get_message(&conn, child).unwrap().unwrap().request_body,
            format!("{gone}child")
        );

        // 数据损坏：解码错误出现在正文标记里，详情其余字段照常
        conn.execute(
            "UPDATE messages SET request_body=X'7F00', response_body=X'0105' WHERE id=?1",
            params![child],
        )
        .unwrap();
        let m = get_message(&conn, child).unwrap().unwrap();
        assert!(m.request_body.starts_with("…[请求/响应体数据损坏：未知编码标签 0x7f]"));
        assert!(m.response_body.starts_with("…[请求/响应体数据损坏：缺少长度前缀]"));
        assert_eq!(m.status, 200);

        // 引用成环：跳数到顶即停，不会死循环
        let a = id("cycle-a");
        let b = id("cycle-b");
        conn.execute("UPDATE messages SET req_ref_id=?2 WHERE id=?1", params![a, b])
            .unwrap();
        conn.execute(
            "UPDATE messages SET req_ref_id=?2, req_prefix_len=100 WHERE id=?1",
            params![b, a],
        )
        .unwrap();
        let m = get_message(&conn, b).unwrap().unwrap().request_body;
        assert!(m.starts_with("…[引用链过长（超过 256 层）"), "{m}");
    }

    #[test]
    fn clear_messages_keeps_refs_closed() {
        let conn = mem();
        let a1 =
            create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        let a2 =
            create_aggregator(&conn, "g2", 8301, "cpm-y", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        let store = BodyStore::new();
        let sys1 = filler(64 * 1024, 0);
        let sys2 = filler(64 * 1024, 1);
        let mut kept = Vec::new();
        for turn in 0..4 {
            for (agg, sys) in [(&a1, &sys1), (&a2, &sys2)] {
                let req = format!("{sys} [turn {turn}]{}", "内容".repeat(500));
                let id = insert_message(&conn, &new_msg(agg.id, &req, "r"), &store)
                    .unwrap()
                    .0;
                if agg.id == a2.id {
                    kept.push((id, req));
                }
            }
        }
        assert_eq!(assert_refs_closed(&conn), 6);

        // 按聚合器清空：删掉的是完整引用闭包，另一聚合器的详情不受影响
        assert_eq!(clear_messages(&conn, Some(a1.id), &store).unwrap(), 4);
        for (id, req) in &kept {
            assert_eq!(get_message(&conn, *id).unwrap().unwrap().request_body, *req);
        }
        // 缓存随之清空：该聚合器之后的新行不会引用已删除的行
        let req = format!("{sys1} after clear");
        let id = insert_message(&conn, &new_msg(a1.id, &req, "r"), &store)
            .unwrap()
            .0;
        assert_eq!(req_ref(&conn, id).0, None);
        assert_eq!(get_message(&conn, id).unwrap().unwrap().request_body, req);

        // 全部清空
        assert_eq!(clear_messages(&conn, None, &store).unwrap(), kept.len() + 1);
        assert_eq!(legacy_message_count(&conn).unwrap(), 0);
        assert!(list_messages(&conn, None, 10, 0).unwrap().1.is_empty());
    }

    #[test]
    fn compress_batch_lossless_idempotent() {
        let conn = mem();
        let a1 =
            create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        let a2 =
            create_aggregator(&conn, "g2", 8301, "cpm-y", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        // 旧库明文行：逐轮增长的会话、完全重复的体、空体、带截断标记的体、多聚合器
        let sys = filler(64 * 1024, 0);
        let mut expected = Vec::new();
        let mut add = |agg: i64, req: String, resp: String| {
            let id = insert_legacy(&conn, agg, &req, &resp);
            expected.push((id, req, resp));
        };
        for turn in 0..5 {
            add(a1.id, format!("{sys}[turn {turn}]{}", "正文".repeat(300)), format!("resp {turn}"));
        }
        add(a1.id, format!("{sys}[turn 0]{}", "正文".repeat(300)), "dup".into());
        add(a1.id, String::new(), String::new());
        add(a1.id, "{\"a\":1}\n…[已截断]".into(), "x".repeat(50_000));
        for turn in 0..3 {
            add(a2.id, format!("{}[b {turn}]", filler(20 * 1024, 3)), "b".into());
        }
        let before = stored_bytes(&conn);
        assert_eq!(legacy_message_count(&conn).unwrap(), expected.len() as i64);

        // 字节预算小于单行：每批一行，多批跑完；中途可续跑（缓存跨批复用）
        let cache = BodyStore::new();
        let mut cursor = 0;
        let (mut rows, mut bytes_in, mut bytes_out) = (0, 0, 0);
        for _ in 0..100 {
            let step = compress_batch(&conn, cursor, 3, 8 * 1024, 3, &cache).unwrap();
            assert!(step.rows <= 3);
            assert!(step.rows > 0, "未完成却没有进展");
            rows += step.rows;
            bytes_in += step.bytes_in;
            bytes_out += step.bytes_out;
            if step.finished {
                break;
            }
            cursor = step.cursor;
        }
        assert_eq!(rows, expected.len());
        assert_eq!(legacy_message_count(&conn).unwrap(), 0);
        assert_eq!(bytes_in as i64, before);
        assert_eq!(bytes_out as i64, stored_bytes(&conn));
        assert!(bytes_out * 4 < before as usize, "压缩后应显著变小：{before} -> {bytes_out}");

        // 无损：逐行与回填前逐字节一致
        for (id, req, resp) in &expected {
            let m = get_message(&conn, *id).unwrap().unwrap();
            assert_eq!(&m.request_body, req, "消息 {id} 请求体");
            assert_eq!(&m.response_body, resp, "消息 {id} 响应体");
        }
        // 全部改为 BLOB；会话行成为增量链，引用不越出同聚合器的更早行
        let text_left = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE typeof(request_body) <> 'blob'
                     OR typeof(response_body) <> 'blob'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(text_left, 0);
        assert!(assert_refs_closed(&conn) >= 4);

        // 幂等：再跑一遍无事可做，数据不变
        let step = compress_batch(&conn, 0, 100, 1 << 30, 3, &BodyStore::new()).unwrap();
        assert_eq!(
            step,
            CompressStep {
                cursor: 0,
                rows: 0,
                bytes_in: 0,
                bytes_out: 0,
                finished: true
            }
        );
        for (id, req, resp) in &expected {
            let m = get_message(&conn, *id).unwrap().unwrap();
            assert_eq!(&m.request_body, req);
            assert_eq!(&m.response_body, resp);
        }
    }

    #[test]
    fn compress_batch_verifies_source_row_exists() {
        // 回填缓存跨批保留：批间被删掉的行仍在缓存里，下一批可能选中它——
        // 落库前确认来源行还在，不在就存整条
        let conn = mem();
        let agg =
            create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        let sys = filler(64 * 1024, 0);
        let p = insert_legacy(&conn, agg.id, &format!("{sys} P"), "r");
        let q = insert_legacy(&conn, agg.id, &format!("{sys} Q"), "r");

        let cache = BodyStore::new();
        let step = compress_batch(&conn, 0, 1, 1 << 30, 3, &cache).unwrap();
        assert_eq!((step.rows, step.cursor, step.finished), (1, p, false));
        assert_eq!(req_ref(&conn, p).0, None);
        conn.execute("DELETE FROM messages WHERE id=?1", params![p]).unwrap();

        let step = compress_batch(&conn, step.cursor, 100, 1 << 30, 3, &cache).unwrap();
        assert!(step.finished);
        assert_eq!(req_ref(&conn, q).0, None);
        assert_eq!(
            get_message(&conn, q).unwrap().unwrap().request_body,
            format!("{sys} Q")
        );
    }

    #[test]
    fn storage_stats_reports_freelist() {
        let conn = mem();
        let agg =
            create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        let big = filler(200 * 1024, 0);
        for i in 0..4 {
            insert_legacy(&conn, agg.id, &format!("{big}{i}"), &"x".repeat(50_000));
        }
        let s = storage_stats(&conn).unwrap();
        assert_eq!(s.page_size, 4096);
        assert_eq!(s.legacy_rows, 4);
        assert_eq!(s.freelist_count, 0);
        assert_eq!(s.reclaimable_bytes(), 0);
        let file = s.file_bytes();
        assert!(file > 800_000, "四个 250KB 的明文行应占约 1MB：{file}");

        // 回填把行改小：腾出的页进空闲表，整理（VACUUM）前文件不会自己变小
        let step = compress_batch(&conn, 0, 100, 1 << 30, 3, &BodyStore::new()).unwrap();
        assert!(step.finished);
        let s = storage_stats(&conn).unwrap();
        assert_eq!(s.legacy_rows, 0);
        assert!(s.file_bytes() >= file);
        assert!(
            s.reclaimable_bytes() > file / 2,
            "大部分页应已空闲：可回收 {} / {file}",
            s.reclaimable_bytes()
        );
    }

    /// 整理途中被取消：SQLite 原子回滚，页统计与内容不变、temp_store 复原；
    /// 之后正常整理才把空闲页还给文件
    #[test]
    fn vacuum_interrupted_midway_keeps_db_intact() {
        let conn = mem();
        let agg =
            create_aggregator(&conn, "g1", 8300, "cpm-x", 100, STRATEGY_THRESHOLD_ROTATION)
                .unwrap();
        let big = filler(200 * 1024, 0);
        let mut rows = Vec::new();
        for i in 0..6 {
            let req = format!("{big}{i}");
            let id = insert_legacy(&conn, agg.id, &req, "p");
            rows.push((id, req));
        }
        compress_batch(&conn, 0, 100, 1 << 30, 3, &BodyStore::new()).unwrap();
        let before = storage_stats(&conn).unwrap();
        let total = list_messages(&conn, None, 100, 0).unwrap().0;

        // 标志已置位 + 回调间隔最小：VACUUM 一开始即被打断
        assert!(!vacuum(&conn, Arc::new(AtomicBool::new(true)), 1).unwrap());

        assert_eq!(storage_stats(&conn).unwrap(), before, "中断的 VACUUM 不改变页统计");
        assert_eq!(list_messages(&conn, None, 100, 0).unwrap().0, total);
        for (id, req) in &rows {
            assert_eq!(get_message(&conn, *id).unwrap().unwrap().request_body, *req);
        }
        let temp_store: i64 = conn
            .query_row("PRAGMA temp_store", [], |r| r.get(0))
            .unwrap();
        assert_eq!(temp_store, 2, "应恢复为 MEMORY");
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");

        assert!(vacuum(&conn, Arc::new(AtomicBool::new(false)), 50_000).unwrap());
        let s = storage_stats(&conn).unwrap();
        assert_eq!(s.freelist_count, 0);
        assert!(s.file_bytes() < before.file_bytes());
        assert_eq!(list_messages(&conn, None, 100, 0).unwrap().0, total);
    }
}
