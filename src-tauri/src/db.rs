use rusqlite::{params, Connection, OptionalExtension, Result};
use serde::{Deserialize, Serialize};

/// 轮转策略标识（当前版本仅实现阈值轮转）
pub const STRATEGY_THRESHOLD_ROTATION: &str = "threshold_rotation";

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
    pub total_tokens: i64,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub requests: i64,
}

/// 分组统计的一个桶（按天 / 按小时 / 按模型），key 为日期、小时或模型名
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsBucket {
    pub key: String,
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub requests: i64,
}

// ---------------------------------------------------------------------------
// 初始化
// ---------------------------------------------------------------------------

pub fn init_db(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA foreign_keys=ON;

        CREATE TABLE IF NOT EXISTS coding_plans (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT NOT NULL,
            base_url    TEXT NOT NULL,
            auth_token  TEXT NOT NULL,
            remark      TEXT NOT NULL DEFAULT '',
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
            total_tokens      INTEGER NOT NULL DEFAULT 0,
            duration_ms       INTEGER NOT NULL DEFAULT 0,
            created_at        TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_messages_agg   ON messages(aggregator_id, id DESC);
        CREATE INDEX IF NOT EXISTS idx_messages_plan  ON messages(plan_id);
        CREATE INDEX IF NOT EXISTS idx_agg_plans_agg  ON aggregator_plans(aggregator_id);
        ",
    )?;
    migrate(&conn)?;
    Ok(conn)
}

/// 旧库增量迁移：为 messages 表补 model 列
fn migrate(conn: &Connection) -> Result<()> {
    let has_model: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name='model'")?
        .query_row([], |row| row.get::<_, i64>(0))? != 0;
    if !has_model {
        conn.execute("ALTER TABLE messages ADD COLUMN model TEXT NOT NULL DEFAULT ''", [])?;
    }
    Ok(())
}

pub fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

// ---------------------------------------------------------------------------
// Coding Plan
// ---------------------------------------------------------------------------

const PLAN_COLS: &str = "id, name, base_url, auth_token, remark, enabled, created_at";

fn row_to_plan(row: &rusqlite::Row) -> Result<CodingPlan> {
    Ok(CodingPlan {
        id: row.get(0)?,
        name: row.get(1)?,
        base_url: row.get(2)?,
        auth_token: row.get(3)?,
        remark: row.get(4)?,
        enabled: row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
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
) -> Result<CodingPlan> {
    let created = now_str();
    conn.execute(
        "INSERT INTO coding_plans (name, base_url, auth_token, remark, enabled, created_at)
         VALUES (?1, ?2, ?3, ?4, 1, ?5)",
        params![name, base_url, auth_token, remark, created],
    )?;
    let id = conn.last_insert_rowid();
    Ok(CodingPlan {
        id,
        name: name.to_string(),
        base_url: base_url.to_string(),
        auth_token: auth_token.to_string(),
        remark: remark.to_string(),
        enabled: true,
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
) -> Result<Option<CodingPlan>> {
    let n = conn.execute(
        "UPDATE coding_plans SET name=?2, base_url=?3, auth_token=?4, remark=?5, enabled=?6 WHERE id=?1",
        params![id, name, base_url, auth_token, remark, enabled as i64],
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

const AGG_COLS: &str = "id, name, port, auth_token, strategy, token_threshold, created_at";

fn row_to_agg(row: &rusqlite::Row) -> Result<Aggregator> {
    Ok(Aggregator {
        id: row.get(0)?,
        name: row.get(1)?,
        port: row.get(2)?,
        auth_token: row.get(3)?,
        strategy: row.get(4)?,
        token_threshold: row.get(5)?,
        created_at: row.get(6)?,
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
) -> Result<Aggregator> {
    let created = now_str();
    conn.execute(
        "INSERT INTO aggregators (name, port, auth_token, strategy, token_threshold, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![name, port, auth_token, STRATEGY_THRESHOLD_ROTATION, token_threshold, created],
    )?;
    let id = conn.last_insert_rowid();
    Ok(Aggregator {
        id,
        name: name.to_string(),
        port,
        auth_token: auth_token.to_string(),
        strategy: STRATEGY_THRESHOLD_ROTATION.to_string(),
        token_threshold,
        created_at: created,
    })
}

pub fn update_aggregator(
    conn: &Connection,
    id: i64,
    name: &str,
    port: i64,
    token_threshold: i64,
) -> Result<Option<Aggregator>> {
    let n = conn.execute(
        "UPDATE aggregators SET name=?2, port=?3, token_threshold=?4 WHERE id=?1",
        params![id, name, port, token_threshold],
    )?;
    if n == 0 {
        return Ok(None);
    }
    get_aggregator(conn, id)
}

pub fn delete_aggregator(conn: &Connection, id: i64) -> Result<usize> {
    conn.execute("DELETE FROM aggregator_plans WHERE aggregator_id=?1", params![id])?;
    let n = conn.execute("DELETE FROM aggregators WHERE id=?1", params![id])?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// 聚合器 <-> 计划 绑定
// ---------------------------------------------------------------------------

pub fn list_bindings(conn: &Connection, aggregator_id: i64) -> Result<Vec<BindingWithPlan>> {
    let mut stmt = conn.prepare(
        "SELECT ap.id, ap.position, ap.used_tokens,
                p.id, p.name, p.base_url, p.auth_token, p.remark, p.enabled, p.created_at
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
                enabled: row.get::<_, i64>(8)? != 0,
                created_at: row.get(9)?,
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
        let mut stmt =
            conn.prepare("SELECT plan_id, used_tokens FROM aggregator_plans WHERE aggregator_id=?1")?;
        let rows = stmt.query_map(params![aggregator_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        for r in rows {
            let (pid, used) = r?;
            old_usage.insert(pid, used);
        }
    }

    conn.execute("DELETE FROM aggregator_plans WHERE aggregator_id=?1", params![aggregator_id])?;
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

pub fn add_binding_usage(conn: &Connection, binding_id: i64, tokens: i64) -> Result<()> {
    conn.execute(
        "UPDATE aggregator_plans SET used_tokens = used_tokens + ?2 WHERE id=?1",
        params![binding_id, tokens],
    )?;
    Ok(())
}

/// 将聚合器下所有绑定的已用 token 清零
pub fn reset_binding_usage(conn: &Connection, aggregator_id: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE aggregator_plans SET used_tokens=0 WHERE aggregator_id=?1",
        params![aggregator_id],
    )?)
}

// ---------------------------------------------------------------------------
// 统计
// ---------------------------------------------------------------------------

fn stats_query(conn: &Connection, where_clause: &str, param: Option<i64>) -> Result<UsageStats> {
    let sql = format!(
        "SELECT COALESCE(SUM(total_tokens),0), COALESCE(SUM(prompt_tokens),0),
                COALESCE(SUM(completion_tokens),0), COUNT(*)
         FROM messages {where_clause}"
    );
    let read = |row: &rusqlite::Row| -> Result<UsageStats> {
        Ok(UsageStats {
            total_tokens: row.get(0)?,
            prompt_tokens: row.get(1)?,
            completion_tokens: row.get(2)?,
            requests: row.get(3)?,
        })
    };
    match param {
        Some(v) => conn.query_row(&sql, params![v], read),
        None => conn.query_row(&sql, [], read),
    }
}

pub fn aggregator_stats(conn: &Connection, aggregator_id: i64) -> Result<UsageStats> {
    stats_query(conn, "WHERE aggregator_id=?1", Some(aggregator_id))
}

pub fn plan_stats(conn: &Connection, plan_id: i64) -> Result<UsageStats> {
    stats_query(conn, "WHERE plan_id=?1", Some(plan_id))
}

pub fn global_stats(conn: &Connection) -> Result<UsageStats> {
    stats_query(conn, "", None)
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
        requests: row.get(4)?,
    })
}

/// 通用分组统计：key_sql 为分组表达式（天/小时/模型），可选聚合器过滤。
/// ordering 为 ORDER BY 表达式（小时/模型分组与 key 顺序不同）。
fn bucket_query(
    conn: &Connection,
    key_sql: &str,
    where_clause: &str,
    ordering: &str,
    param: Option<i64>,
) -> Result<Vec<StatsBucket>> {
    let sql = format!(
        "SELECT {key_sql} AS k, COALESCE(SUM(total_tokens),0), COALESCE(SUM(prompt_tokens),0), \
         COALESCE(SUM(completion_tokens),0), COUNT(*) \
         FROM messages {where_clause} GROUP BY k ORDER BY {ordering}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = match param {
        Some(v) => stmt.query_map(params![v], row_to_bucket)?.collect::<Result<Vec<_>>>()?,
        None => stmt.query_map([], row_to_bucket)?.collect::<Result<Vec<_>>>()?,
    };
    Ok(rows)
}

/// 按天统计：最近 days 天（含今天），created_at 为本地时间 "YYYY-MM-DD HH:MM:SS"
pub fn stats_daily(conn: &Connection, aggregator_id: Option<i64>, days: i64) -> Result<Vec<StatsBucket>> {
    let cutoff = (chrono::Local::now() - chrono::Duration::days(days - 1))
        .format("%Y-%m-%d")
        .to_string();
    let (where_clause, param) = match aggregator_id {
        Some(id) => (format!("WHERE aggregator_id=?1 AND substr(created_at,1,10)>=('{cutoff}')"), Some(id)),
        None => (format!("WHERE substr(created_at,1,10)>=('{cutoff}')"), None),
    };
    bucket_query(
        conn,
        "substr(created_at,1,10)",
        &where_clause,
        "k",
        param,
    )
}

/// 按小时统计：指定日期（"YYYY-MM-DD"）内 0-23 点各小时用量
pub fn stats_hourly(conn: &Connection, aggregator_id: Option<i64>, date: &str) -> Result<Vec<StatsBucket>> {
    let (where_clause, param) = match aggregator_id {
        Some(id) => (
            format!("WHERE aggregator_id=?1 AND substr(created_at,1,10)=('{date}')"),
            Some(id),
        ),
        None => (format!("WHERE substr(created_at,1,10)=('{date}')"), None),
    };
    bucket_query(
        conn,
        "substr(created_at,12,2)",
        &where_clause,
        "CAST(k AS INTEGER)",
        param,
    )
}

/// 按模型统计：按总 token 倒序
pub fn stats_by_model(conn: &Connection, aggregator_id: Option<i64>) -> Result<Vec<StatsBucket>> {
    let (where_clause, param) = match aggregator_id {
        Some(id) => ("WHERE aggregator_id=?1".to_string(), Some(id)),
        None => (String::new(), None),
    };
    bucket_query(
        conn,
        "model",
        &where_clause,
        "COALESCE(SUM(total_tokens),0) DESC, k",
        param,
    )
}

// ---------------------------------------------------------------------------
// 消息
// ---------------------------------------------------------------------------

const MSG_COLS: &str = "m.id, m.aggregator_id, m.plan_id, m.method, m.path, m.status, \
                        m.request_body, m.response_body, m.model, m.prompt_tokens, \
                        m.completion_tokens, m.total_tokens, m.duration_ms, m.created_at, \
                        a.name, p.name";

fn row_to_message(row: &rusqlite::Row) -> Result<MessageRow> {
    Ok(MessageRow {
        id: row.get(0)?,
        aggregator_id: row.get(1)?,
        plan_id: row.get(2)?,
        method: row.get(3)?,
        path: row.get(4)?,
        status: row.get(5)?,
        request_body: row.get(6)?,
        response_body: row.get(7)?,
        model: row.get(8)?,
        prompt_tokens: row.get(9)?,
        completion_tokens: row.get(10)?,
        total_tokens: row.get(11)?,
        duration_ms: row.get(12)?,
        created_at: row.get(13)?,
        aggregator_name: row.get(14)?,
        plan_name: row.get(15)?,
    })
}

pub fn insert_message(conn: &Connection, msg: &NewMessage) -> Result<i64> {
    conn.execute(
        "INSERT INTO messages (aggregator_id, plan_id, method, path, status, request_body,
                               response_body, model, prompt_tokens, completion_tokens,
                               total_tokens, duration_ms, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            msg.aggregator_id,
            msg.plan_id,
            msg.method,
            msg.path,
            msg.status,
            msg.request_body,
            msg.response_body,
            msg.model,
            msg.prompt_tokens,
            msg.completion_tokens,
            msg.total_tokens,
            msg.duration_ms,
            now_str()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_message(conn: &Connection, id: i64) -> Result<Option<MessageRow>> {
    let sql = format!(
        "SELECT {MSG_COLS} FROM messages m
         LEFT JOIN aggregators a ON a.id = m.aggregator_id
         LEFT JOIN coding_plans p ON p.id = m.plan_id
         WHERE m.id=?1"
    );
    conn.query_row(&sql, params![id], row_to_message).optional()
}

/// 分页列出消息（按 id 倒序），返回 (总数, 本页数据)
pub fn list_messages(
    conn: &Connection,
    aggregator_id: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<MessageRow>)> {
    let (count_sql, list_sql) = match aggregator_id {
        Some(_agg_id) => (
            "SELECT COUNT(*) FROM messages WHERE aggregator_id=?1".to_string(),
            format!(
                "SELECT {MSG_COLS} FROM messages m
                 LEFT JOIN aggregators a ON a.id = m.aggregator_id
                 LEFT JOIN coding_plans p ON p.id = m.plan_id
                 WHERE m.aggregator_id=?1 ORDER BY m.id DESC LIMIT ?2 OFFSET ?3"
            ),
        ),
        None => (
            "SELECT COUNT(*) FROM messages".to_string(),
            format!(
                "SELECT {MSG_COLS} FROM messages m
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
    let rows = stmt.query_map(
        params![aggregator_id, limit, offset],
        row_to_message,
    )?;
    let items = rows.collect::<Result<Vec<_>>>()?;
    Ok((total, items))
}

pub fn clear_messages(conn: &Connection, aggregator_id: Option<i64>) -> Result<usize> {
    match aggregator_id {
        Some(id) => Ok(conn.execute("DELETE FROM messages WHERE aggregator_id=?1", params![id])?),
        None => Ok(conn.execute("DELETE FROM messages", [])?),
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
        let p = create_plan(&conn, "p1", "https://a.example", "tok1", "r").unwrap();
        assert_eq!(p.enabled, true);
        let got = get_plan(&conn, p.id).unwrap().unwrap();
        assert_eq!(got.name, "p1");
        update_plan(&conn, p.id, "p1x", "https://b.example", "tok2", "r2", false).unwrap();
        let got = get_plan(&conn, p.id).unwrap().unwrap();
        assert_eq!(got.name, "p1x");
        assert_eq!(got.enabled, false);
        assert_eq!(delete_plan(&conn, p.id).unwrap(), 1);
        assert!(get_plan(&conn, p.id).unwrap().is_none());
    }

    #[test]
    fn bindings_and_reset() {
        let conn = mem();
        let p1 = create_plan(&conn, "p1", "https://a", "t1", "").unwrap();
        let p2 = create_plan(&conn, "p2", "https://b", "t2", "").unwrap();
        let agg = create_aggregator(&conn, "g1", 8300, "cpm-x", 100).unwrap();
        set_aggregator_plans(&conn, agg.id, &[p1.id, p2.id]).unwrap();

        let bs = list_bindings(&conn, agg.id).unwrap();
        assert_eq!(bs.len(), 2);
        assert_eq!(bs[0].plan.id, p1.id);
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
        let p1 = create_plan(&conn, "p1", "https://a", "t1", "").unwrap();
        let agg = create_aggregator(&conn, "g1", 8300, "cpm-x", 100).unwrap();
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
                    total_tokens: 15,
                    duration_ms: 3,
                },
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
        assert_eq!(s.total_tokens, 75);
        let s = plan_stats(&conn, p1.id).unwrap();
        assert_eq!(s.total_tokens, 75);
        let s = global_stats(&conn).unwrap();
        assert_eq!(s.total_tokens, 75);

        assert_eq!(clear_messages(&conn, Some(agg.id)).unwrap(), 5);
        assert_eq!(global_stats(&conn).unwrap().requests, 0);
    }

    #[test]
    fn grouped_stats_and_migration() {
        let conn = mem();
        let p1 = create_plan(&conn, "p1", "https://a", "t1", "").unwrap();
        let agg = create_aggregator(&conn, "g1", 8300, "cpm-x", 100).unwrap();
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
                    total_tokens: prompt + completion,
                    duration_ms: 1,
                },
                created.to_string(),
            )
        };
        let insert_at = |m: NewMessage, created: String| {
            insert_message(&conn, &m).unwrap();
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
        let models = stats_by_model(&conn, None).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].key, "glm-4.7");
        assert_eq!(models[0].total_tokens, 150 + 300 + 1000);
        assert_eq!(models[1].key, "claude-sonnet-5");
        assert_eq!(models[1].requests, 1);

        // 聚合器过滤（其他聚合器 id 应为空）
        assert!(stats_daily(&conn, Some(agg.id + 100), 1).unwrap().is_empty());

        // 迁移：旧库（无 model 列）打开后自动补列
        let old = Connection::open_in_memory().unwrap();
        old.execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT, aggregator_id INTEGER NOT NULL,
                plan_id INTEGER, method TEXT NOT NULL DEFAULT '', path TEXT NOT NULL DEFAULT '',
                status INTEGER NOT NULL DEFAULT 0, request_body TEXT NOT NULL DEFAULT '',
                response_body TEXT NOT NULL DEFAULT '', prompt_tokens INTEGER NOT NULL DEFAULT 0,
                completion_tokens INTEGER NOT NULL DEFAULT 0, total_tokens INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL);",
        )
        .unwrap();
        migrate(&old).unwrap();
        migrate(&old).unwrap(); // 幂等
        old.execute(
            "INSERT INTO messages (aggregator_id, created_at) VALUES (1, '2026-08-17 10:00:00')",
            [],
        )
        .unwrap();
        let buckets = stats_by_model(&old, None).unwrap();
        assert_eq!(buckets[0].key, ""); // 旧行 model 为空串
    }
}
