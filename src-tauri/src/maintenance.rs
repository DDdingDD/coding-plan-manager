//! 库维护任务：历史消息回填压缩、库文件整理（VACUUM）。
//!
//! 同一时刻只运行一个任务，在命名线程 `cpm-maintenance` 上执行。与命令层/代理共用
//! 同一个数据库连接，但每批单独取锁、做完即放（回填）；整理文件则是 SQLite 自身的
//! 原子操作，全程独占连接，因此要求此时没有聚合器在转发。
//!
//! 任务状态记在本模块的 Mutex 里：进度查询与取消都不需要数据库锁——整理期间界面
//! 仍能每秒轮询。不依赖 tauri，便于单测。

use crate::body::BodyStore;
use crate::db;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// 回填压缩级别：不在请求路径上，用高于在线的级别换更小体积。
/// 同一批 5000 行 / 2GB 真实数据的实测（release）：级别 12 耗时 54s、级别 3 耗时 21s，
/// 存储态 134.8MB 对 144.6MB——一次性任务，多花半分钟换长期小 7%，取 12
pub const BACKFILL_LEVEL: i32 = 12;

/// 有聚合器在转发时的批量上限：单次持锁约 150~250ms，代理请求最多等一批；
/// 批间歇给排队的请求留出空档
const BUSY_MAX_ROWS: usize = 8;
const BUSY_MAX_BYTES: usize = 4 << 20;
const BUSY_SLEEP: Duration = Duration::from_millis(50);
/// 无聚合器运行时只按字节封顶，连批处理不休眠
const IDLE_MAX_ROWS: usize = 4_096;
const IDLE_MAX_BYTES: usize = 16 << 20;
/// 整理文件的进度回调间隔（VM 指令数）：仅为及时读到取消标志
const VACUUM_CHECK_EVERY: i32 = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Compress,
    Vacuum,
}

impl JobKind {
    pub fn name(self) -> &'static str {
        match self {
            JobKind::Compress => "压缩历史数据",
            JobKind::Vacuum => "整理文件",
        }
    }
}

/// 进行中任务的快照（前端轮询用）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobView {
    pub kind: JobKind,
    pub elapsed_ms: u64,
    /// 估算完成度 0..1。整理文件得知不了总量，恒为 0
    pub fraction: f64,
    /// 当前阶段的中文说明
    pub stage: String,
    /// 已处理的行数 / 压缩前后字节数（整理文件恒为 0）
    pub rows_done: usize,
    pub bytes_in: usize,
    pub bytes_out: usize,
    /// 已请求取消，等待当前批结束
    pub cancelling: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Cancelled,
    Failed,
}

/// 最近一次任务的结局（完成/取消/失败都记；应用重启后清空）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResult {
    pub kind: JobKind,
    pub outcome: Outcome,
    /// 面向用户的说明
    pub message: String,
    pub elapsed_ms: u64,
}

impl JobResult {
    fn new(kind: JobKind, outcome: Outcome, message: String, started: Instant) -> Self {
        Self {
            kind,
            outcome,
            message,
            elapsed_ms: started.elapsed().as_millis() as u64,
        }
    }
}

struct Running {
    started: Instant,
    view: JobView,
}

#[derive(Default)]
struct State {
    running: Option<Running>,
    last: Option<JobResult>,
}

pub struct Maintenance {
    state: Mutex<State>,
    cancel: Arc<AtomicBool>,
}

impl Default for Maintenance {
    fn default() -> Self {
        Self::new()
    }
}

impl Maintenance {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // panic=abort 下不会中毒，仍按既有惯例恢复而非 unwrap
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 登记一个任务；已有任务在跑则拒绝。同时清掉上一次遗留的取消标志
    fn try_start(&self, kind: JobKind) -> Result<Running, String> {
        let mut s = self.lock();
        if s.running.is_some() {
            return Err("已有维护任务正在进行".into());
        }
        self.cancel.store(false, Ordering::Relaxed);
        let running = Running {
            started: Instant::now(),
            view: JobView {
                kind,
                elapsed_ms: 0,
                fraction: 0.0,
                stage: "正在启动".into(),
                rows_done: 0,
                bytes_in: 0,
                bytes_out: 0,
                cancelling: false,
            },
        };
        s.running = Some(Running {
            started: running.started,
            view: running.view.clone(),
        });
        Ok(running)
    }

    /// 收尾：清除运行标记并记录结果
    fn finish(&self, result: JobResult) {
        let mut s = self.lock();
        s.running = None;
        s.last = Some(result);
    }

    /// 启动「压缩历史数据」：把请求体仍为明文的消息行改写为编码存储（无损）。
    /// db 为应用共享连接；proxy_busy 判断此刻是否有聚合器在转发（决定批量与节流）。
    /// 立即返回，任务在后台线程运行
    pub fn start_compress(
        self: &Arc<Self>,
        db: Arc<Mutex<Connection>>,
        proxy_busy: Arc<dyn Fn() -> bool + Send + Sync>,
        level: i32,
    ) -> Result<(), String> {
        self.try_start(JobKind::Compress)?;
        self.spawn(JobKind::Compress, move |this| {
            run_compress(&db, &*proxy_busy, level, this)
        })
    }

    /// 启动「整理文件」（VACUUM）。调用方须先确认没有聚合器在运行：
    /// 整理全程独占数据库连接，运行中的代理请求会挂起数分钟
    pub fn start_vacuum(self: &Arc<Self>, db: Arc<Mutex<Connection>>) -> Result<(), String> {
        self.try_start(JobKind::Vacuum)?;
        self.spawn(JobKind::Vacuum, move |this| run_vacuum(&db, this))
    }

    fn spawn(
        self: &Arc<Self>,
        kind: JobKind,
        run: impl FnOnce(&Maintenance) -> JobResult + Send + 'static,
    ) -> Result<(), String> {
        let this = Arc::clone(self);
        let started = Instant::now();
        let spawned = thread::Builder::new()
            .name("cpm-maintenance".into())
            .spawn(move || {
                // panic=abort 的发布版走不到 catch_unwind；调试版兜住，
                // 保证运行标记一定被清除、界面不会永远显示"进行中"
                let result = catch_unwind(AssertUnwindSafe(|| run(&this)))
                    .unwrap_or_else(|_| JobResult::new(kind, Outcome::Failed, "任务异常终止".into(), started));
                this.finish(result);
            });
        if let Err(e) = spawned {
            let msg = format!("无法启动后台线程：{e}");
            self.finish(JobResult::new(kind, Outcome::Failed, msg.clone(), started));
            return Err(msg);
        }
        Ok(())
    }

    /// 请求取消当前任务：在当前批结束后生效（整理文件为 SQLite 原子中断）。
    /// 返回是否确有任务在运行
    pub fn request_cancel(&self) -> bool {
        if self.lock().running.is_none() {
            return false;
        }
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(r) = self.lock().running.as_mut() {
            r.view.cancelling = true;
        }
        true
    }

    /// 进行中任务的快照（已用时按当前时刻计算）
    pub fn snapshot(&self) -> Option<JobView> {
        let s = self.lock();
        let r = s.running.as_ref()?;
        let mut view = r.view.clone();
        view.elapsed_ms = r.started.elapsed().as_millis() as u64;
        Some(view)
    }

    /// 最近一次任务的结局（应用生命周期内保留）
    pub fn last_result(&self) -> Option<JobResult> {
        self.lock().last.clone()
    }

    /// 是否正在整理文件：期间不能启动聚合器，其请求会在数据库锁上挂起数分钟
    pub fn is_vacuuming(&self) -> bool {
        matches!(
            self.lock().running.as_ref().map(|r| r.view.kind),
            Some(JobKind::Vacuum)
        )
    }

    /// 更新进度（任务线程调用）
    fn report(&self, update: impl FnOnce(&mut JobView)) {
        if let Some(r) = self.lock().running.as_mut() {
            update(&mut r.view);
        }
    }
}

fn lock(db: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    db.lock().unwrap_or_else(|p| p.into_inner())
}

/// 面向用户的字节数描述（保留一位小数）
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// 回填循环：按 id 升序逐批调用 db::compress_batch，每批单独取锁。
/// 取消在批间生效；当前批已提交的行保持有效，未处理的行仍是明文，可续跑
fn run_compress(
    db: &Mutex<Connection>,
    proxy_busy: &dyn Fn() -> bool,
    level: i32,
    m: &Maintenance,
) -> JobResult {
    let started = Instant::now();
    let cancel = &*m.cancel;
    let kind = JobKind::Compress;
    // 任务私有的近期请求体缓存（不进 AppState.bodies：那份是在线落库专用）
    let cache = BodyStore::new();
    let total = {
        let conn = lock(db);
        match db::legacy_message_count(&conn) {
            Ok(t) => t,
            Err(e) => {
                return JobResult::new(kind, Outcome::Failed, format!("无法读取待压缩消息数：{e}"), started)
            }
        }
    };
    if total == 0 {
        return JobResult::new(kind, Outcome::Completed, "没有需要压缩的历史消息".into(), started);
    }
    let mut cursor = 0;
    let (mut rows_done, mut bytes_in, mut bytes_out) = (0, 0, 0);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return JobResult::new(kind, Outcome::Cancelled, cancel_message(rows_done), started);
        }
        let busy = proxy_busy();
        let (max_rows, max_bytes) = if busy {
            (BUSY_MAX_ROWS, BUSY_MAX_BYTES)
        } else {
            (IDLE_MAX_ROWS, IDLE_MAX_BYTES)
        };
        let stepped = {
            let conn = lock(db);
            db::compress_batch(&conn, cursor, max_rows, max_bytes, level, &cache)
                .and_then(|s| db::legacy_message_count(&conn).map(|r| (s, r)))
        };
        let (step, remaining) = match stepped {
            Ok(x) => x,
            Err(e) => {
                let msg = format!(
                    "压缩中断（数据库错误：{e}）。已压缩的行保持有效，再次压缩会从剩余明文行继续"
                );
                return JobResult::new(kind, Outcome::Failed, msg, started);
            }
        };
        cursor = step.cursor;
        rows_done += step.rows;
        bytes_in += step.bytes_in;
        bytes_out += step.bytes_out;
        let done = (total - remaining).max(0);
        m.report(|v| {
            v.rows_done = rows_done;
            v.bytes_in = bytes_in;
            v.bytes_out = bytes_out;
            v.fraction = (1.0 - remaining as f64 / total as f64).clamp(0.0, 1.0);
            v.stage = format!("已压缩 {done} / {total} 条");
        });
        if step.finished {
            break;
        }
        if step.rows == 0 {
            // 游标之后仍有明文行却一行未处理：正常不会发生，防死循环
            let msg = "压缩未取得进展，已停止（已压缩的行保持有效）".to_string();
            return JobResult::new(kind, Outcome::Failed, msg, started);
        }
        if busy {
            thread::sleep(BUSY_SLEEP);
        }
    }
    let msg = format!(
        "已压缩 {rows_done} 条消息：请求体 + 响应体 {} → {}",
        fmt_bytes(bytes_in as u64),
        fmt_bytes(bytes_out as u64)
    );
    JobResult::new(kind, Outcome::Completed, msg, started)
}

fn cancel_message(rows_done: usize) -> String {
    format!("已取消：本次已压缩 {rows_done} 条，其余仍为明文，可再次压缩继续")
}

/// 整理文件：VACUUM 全程独占连接。取消标志在开始前与执行途中（进度回调）都会被读取；
/// 中断是 SQLite 的原子回滚，库保持原样
fn run_vacuum(db: &Mutex<Connection>, m: &Maintenance) -> JobResult {
    let started = Instant::now();
    let kind = JobKind::Vacuum;
    m.report(|v| v.stage = "正在重写数据库文件，期间数据库完全锁定".into());
    let conn = lock(db);
    let untouched = "已取消：数据库文件未被改动".to_string();
    if m.cancel.load(Ordering::Relaxed) {
        return JobResult::new(kind, Outcome::Cancelled, untouched, started);
    }
    let before = db::storage_stats(&conn).map(|s| s.file_bytes()).unwrap_or(0);
    match db::vacuum(&conn, Arc::clone(&m.cancel), VACUUM_CHECK_EVERY) {
        Ok(true) => {
            let after = db::storage_stats(&conn)
                .map(|s| s.file_bytes())
                .unwrap_or(0);
            let msg = format!(
                "数据库文件 {} → {}",
                fmt_bytes(before as u64),
                fmt_bytes(after as u64)
            );
            JobResult::new(kind, Outcome::Completed, msg, started)
        }
        Ok(false) => JobResult::new(kind, Outcome::Cancelled, untouched, started),
        Err(e) => {
            let msg = format!("整理失败（数据库错误：{e}），数据库未被改动");
            JobResult::new(kind, Outcome::Failed, msg, started)
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    /// 等待后台任务结束的轮询上限（×10ms）
    const WAIT_ROUNDS: usize = 3_000;

    /// 临时文件库（WAL 生效，文件长度反映 VACUUM 效果）；Drop 时关闭连接并删除全部相关文件
    struct TempDb {
        conn: Option<Arc<Mutex<Connection>>>,
        path: PathBuf,
    }

    impl TempDb {
        fn new(name: &str) -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "cpm-maint-{name}-{}-{id}.db",
                std::process::id()
            ));
            remove_files(&path);
            let conn = db::init_db(path.to_string_lossy().as_ref()).unwrap();
            Self {
                conn: Some(Arc::new(Mutex::new(conn))),
                path,
            }
        }

        /// 共享连接（任务线程经 Arc 克隆持有，测试经借用持锁）
        fn conn(&self) -> &Arc<Mutex<Connection>> {
            self.conn.as_ref().expect("连接已关闭")
        }

        /// 主库文件长度
        fn file_len(&self) -> u64 {
            std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
        }

        /// 主库 + WAL 的磁盘占用（WAL 模式下新写入先进 WAL，尚未 checkpoint 的页不在主文件里）
        fn disk_len(&self) -> u64 {
            let wal = std::fs::metadata(format!("{}-wal", self.path.display()))
                .map(|m| m.len())
                .unwrap_or(0);
            self.file_len() + wal
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            // Windows 上须先关连接才能删文件（任务线程已结束，此处是最后一个持有者）
            if let Some(conn) = self.conn.take() {
                if let Ok(m) = Arc::try_unwrap(conn) {
                    drop(m.into_inner());
                }
            }
            remove_files(&self.path);
        }
    }

    fn remove_files(path: &PathBuf) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    /// 测试用：读一个返回整数的 PRAGMA
    fn pragma(conn: &Connection, name: &str) -> i64 {
        conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0)).unwrap()
    }

    fn legacy_rows(db: &TempDb) -> i64 {
        db::legacy_message_count(&lock(&db.conn())).unwrap()
    }

    /// 直接写入一条明文（旧库形态）消息，返回行 id
    fn insert_legacy(conn: &Connection, req: &str) -> i64 {
        conn.execute(
            "INSERT INTO messages (aggregator_id, request_body, response_body, created_at)
             VALUES (1, ?1, 'resp', '2026-01-01 00:00:00')",
            params![req],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn filler(len: usize, seed: u8) -> String {
        let mut s = String::with_capacity(len + 8);
        while s.len() < len {
            s.push((b'a' + (s.len() as u8 + seed) % 26) as char);
        }
        s
    }

    fn wait_finished(m: &Maintenance) -> JobResult {
        for _ in 0..WAIT_ROUNDS {
            if m.snapshot().is_none() {
                return m.last_result().expect("任务结束后应有结果");
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("维护任务未在预期时间内结束");
    }

    /// 填入 count 条「公共前缀 + 轮次后缀」的明文行，返回 (行 id, 请求体)
    fn seed_legacy(db: &TempDb, count: usize, body_len: usize) -> Vec<(i64, String)> {
        let sys = filler(body_len, 0);
        let conn = lock(&db.conn());
        (0..count)
            .map(|turn| {
                let req = format!("{sys}[turn {turn}]{}", "正文".repeat(200));
                let id = insert_legacy(&conn, &req);
                (id, req)
            })
            .collect()
    }

    fn assert_rows_intact(db: &TempDb, rows: &[(i64, String)]) {
        let conn = lock(&db.conn());
        for (id, req) in rows {
            let m = db::get_message(&conn, *id).unwrap().unwrap();
            assert_eq!(m.request_body, *req, "行 {id} 请求体应逐字节一致");
            assert_eq!(m.response_body, "resp");
        }
    }

    #[test]
    fn compress_job_progress_and_done() {
        let db = TempDb::new("progress");
        let rows = seed_legacy(&db, 40, 64 * 1024);
        // 混入不足增量阈值的短行与空行
        {
            let conn = lock(&db.conn());
            insert_legacy(&conn, "tiny");
            insert_legacy(&conn, "");
        }
        let m = Arc::new(Maintenance::new());
        let checked = Arc::new(AtomicBool::new(false));
        let busy = {
            let m = Arc::clone(&m);
            let checked = Arc::clone(&checked);
            Arc::new(move || {
                // 任务线程里：此刻快照必然可见且为压缩任务
                let v = m.snapshot().expect("运行中应有快照");
                assert_eq!(v.kind, JobKind::Compress);
                assert!(!m.is_vacuuming());
                checked.store(true, Ordering::Relaxed);
                false
            })
        };
        m.start_compress(Arc::clone(&db.conn()), busy, BACKFILL_LEVEL)
            .unwrap();

        let r = wait_finished(&m);
        assert_eq!(r.outcome, Outcome::Completed, "{}", r.message);
        assert!(r.message.contains("已压缩 42 条"), "{}", r.message);
        assert!(checked.load(Ordering::Relaxed), "任务线程应调用过 proxy_busy");
        assert!(m.snapshot().is_none());
        assert_eq!(m.last_result().as_ref(), Some(&r));

        assert_eq!(legacy_rows(&db), 0);
        assert_rows_intact(&db, &rows);
        // 行改小后留下空闲页，等整理回收
        let stats = db::storage_stats(&lock(&db.conn())).unwrap();
        assert!(stats.reclaimable_bytes() > 0);
        assert!(stats.file_bytes() > 0);
    }

    #[test]
    fn compress_job_cancel_resumes() {
        let db = TempDb::new("cancel");
        let total = 300;
        let rows = seed_legacy(&db, total, 64 * 1024);
        let m = Arc::new(Maintenance::new());
        // 有聚合器在转发：小批量 + 节流。第 3 次询问时请求取消——它在第 3 批之前
        // 发生，取消在第 4 批之前生效，故恰有 3 批（每批 BUSY_MAX_ROWS 行）被压缩
        let calls = Arc::new(AtomicU64::new(0));
        let busy = {
            let m = Arc::clone(&m);
            let calls = Arc::clone(&calls);
            Arc::new(move || {
                if calls.fetch_add(1, Ordering::Relaxed) + 1 == 3 {
                    assert!(m.snapshot().unwrap().cancelling || {
                        m.request_cancel();
                        true
                    });
                }
                true
            })
        };
        m.start_compress(Arc::clone(&db.conn()), busy, 3).unwrap();
        let r = wait_finished(&m);
        assert_eq!(r.outcome, Outcome::Cancelled, "{}", r.message);
        let done = 3 * BUSY_MAX_ROWS as i64;
        assert_eq!(
            legacy_rows(&db),
            (total as i64) - done,
            "恰有 3 批被压缩"
        );
        assert!(r.message.contains(&format!("已压缩 {done} 条")), "{}", r.message);
        assert_rows_intact(&db, &rows);

        // 再次压缩：从剩余明文行继续直到全部完成
        let m = Arc::new(Maintenance::new());
        m.start_compress(Arc::clone(&db.conn()), Arc::new(|| false), 3)
            .unwrap();
        let r = wait_finished(&m);
        assert_eq!(r.outcome, Outcome::Completed, "{}", r.message);
        assert_eq!(legacy_rows(&db), 0);
        assert_rows_intact(&db, &rows);
    }

    #[test]
    fn vacuum_shrinks_and_restores_pragmas() {
        let db = TempDb::new("vacuum");
        let rows = seed_legacy(&db, 24, 256 * 1024);
        let m = Arc::new(Maintenance::new());
        m.start_compress(Arc::clone(&db.conn()), Arc::new(|| false), BACKFILL_LEVEL)
            .unwrap();
        assert_eq!(wait_finished(&m).outcome, Outcome::Completed);
        // 6MB 明文行压成几十 KB，但页只是进了空闲列表，磁盘占用不变
        let before = db.disk_len();
        assert!(before > 4 << 20, "压缩后空闲页仍占着文件：{before}");
        assert!(db::storage_stats(&lock(&db.conn())).unwrap().reclaimable_bytes() > 0);

        m.start_vacuum(Arc::clone(&db.conn())).unwrap();
        assert!(m.is_vacuuming());
        let r = wait_finished(&m);
        assert_eq!(r.outcome, Outcome::Completed, "{}", r.message);
        assert!(!m.is_vacuuming());

        let after = db.disk_len();
        assert!(
            after < before / 2,
            "整理后文件应明显缩小：{before} -> {after}"
        );
        {
            let conn = lock(&db.conn());
            assert_eq!(pragma(&conn, "freelist_count"), 0);
            assert_eq!(pragma(&conn, "temp_store"), 2, "应恢复为 MEMORY");
            let check: String = conn
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap();
            assert_eq!(check, "ok");
            // WAL 已收回：主文件长度即页数 × 页大小
            let stats = db::storage_stats(&conn).unwrap();
            assert_eq!(stats.file_bytes(), after as i64);
            assert_eq!(db::legacy_message_count(&conn).unwrap(), 0);
        }
        assert_rows_intact(&db, &rows);
    }

    #[test]
    fn vacuum_cancel_leaves_db_intact() {
        let db = TempDb::new("vacuum-cancel");
        let rows = seed_legacy(&db, 8, 256 * 1024);
        let m = Arc::new(Maintenance::new());
        // 占住数据库锁启动任务：任务线程卡在取锁处，此刻取消必然先于 VACUUM 开始
        let guard = lock(&db.conn());
        m.start_vacuum(Arc::clone(&db.conn())).unwrap();
        assert!(m.is_vacuuming());
        assert!(m.request_cancel());
        assert!(m.snapshot().unwrap().cancelling);
        let (freelist, count) = (pragma(&guard, "freelist_count"), db::list_messages(&guard, None, 1, 0).unwrap().0);
        drop(guard);

        let r = wait_finished(&m);
        assert_eq!(r.outcome, Outcome::Cancelled, "{}", r.message);
        assert!(!m.request_cancel(), "没有任务时取消返回 false");
        {
            let conn = lock(&db.conn());
            assert_eq!(pragma(&conn, "freelist_count"), freelist);
            assert_eq!(pragma(&conn, "temp_store"), 2);
            assert_eq!(db::list_messages(&conn, None, 1, 0).unwrap().0, count);
            let check: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
            assert_eq!(check, "ok");
        }
        assert_rows_intact(&db, &rows);
    }

    #[test]
    fn try_start_rejects_second_job() {
        let db = TempDb::new("single");
        seed_legacy(&db, 4, 32 * 1024);
        let m = Arc::new(Maintenance::new());
        let guard = lock(&db.conn());
        m.start_compress(Arc::clone(&db.conn()), Arc::new(|| false), 3)
            .unwrap();
        assert!(m.snapshot().is_some());
        assert_eq!(
            m.start_vacuum(Arc::clone(&db.conn())).unwrap_err(),
            "已有维护任务正在进行"
        );
        assert!(m.request_cancel());
        drop(guard);
        assert_eq!(wait_finished(&m).outcome, Outcome::Cancelled);

        // 上一个任务结束后可以启动
        m.start_vacuum(Arc::clone(&db.conn())).unwrap();
        assert_eq!(wait_finished(&m).outcome, Outcome::Completed);
        assert!(m.snapshot().is_none());
    }

    #[test]
    fn fmt_bytes_units() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.0 KB");
        assert_eq!(fmt_bytes(1 << 20), "1.0 MB");
        assert_eq!(fmt_bytes((34.6 * 1073741824.0) as u64), "34.6 GB");
        assert_eq!(fmt_bytes(1 << 40), "1.0 TB");
    }
}
