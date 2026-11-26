//! 对真实库副本的端到端验证（默认忽略）：迁移 → 回填压缩 → 逐行哈希比对 → 整理 → 再比对。
//!
//! 用法：
//!   CPM_REAL_DB_COPY=D:\cpm-work\cpm.db cargo test --manifest-path src-tauri/Cargo.toml \
//!     --test real_db -- --ignored --nocapture
//! 可选 CPM_BACKFILL_LEVEL（默认取 maintenance::BACKFILL_LEVEL）。
//!
//! 只接受副本：路径含 `com.codingplan.manager`（真实库所在目录）一律拒绝。

use coding_plan_manager_lib::{db, maintenance, maintenance::Outcome};
use rusqlite::types::ValueRef;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn target() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("CPM_REAL_DB_COPY").expect("需设置 CPM_REAL_DB_COPY 指向库副本"),
    );
    assert!(
        !path.to_string_lossy().contains("com.codingplan.manager"),
        "拒绝操作真实库：{}",
        path.display()
    );
    assert!(path.is_file(), "副本不存在：{}", path.display());
    path
}

fn level() -> i32 {
    std::env::var("CPM_BACKFILL_LEVEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(maintenance::BACKFILL_LEVEL)
}

/// 主库 + WAL 的磁盘占用
fn disk_len(path: &Path) -> u64 {
    ["", "-wal"]
        .iter()
        .map(|s| {
            std::fs::metadata(format!("{}{s}", path.display()))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .sum()
}

fn hash_body(h: &mut DefaultHasher, v: ValueRef<'_>) -> usize {
    match v {
        ValueRef::Text(b) | ValueRef::Blob(b) => {
            b.hash(h);
            b.len()
        }
        ValueRef::Null => 0,
        other => {
            format!("{other:?}").hash(h);
            0
        }
    }
}

/// 压缩前（明文形态）逐行哈希：query_only 连接流式读取，不把整库装进内存。
/// 返回 (行 id → 请求体+响应体哈希, body 总字节数)
fn hash_raw(path: &Path) -> (HashMap<i64, u64>, usize) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA query_only=ON").unwrap();
    let mut stmt = conn
        .prepare("SELECT id, request_body, response_body FROM messages ORDER BY id")
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut map = HashMap::new();
    let mut total = 0;
    while let Some(row) = rows.next().unwrap() {
        let id: i64 = row.get(0).unwrap();
        let mut h = DefaultHasher::new();
        total += hash_body(&mut h, row.get_ref(1).unwrap());
        total += hash_body(&mut h, row.get_ref(2).unwrap());
        map.insert(id, h.finish());
    }
    (map, total)
}

/// 压缩后经应用读路径（get_message）逐行哈希，与压缩前比对；返回存储态 body 总字节数
fn verify_decoded(conn: &Arc<Mutex<rusqlite::Connection>>, before: &HashMap<i64, u64>) -> usize {
    let mut ids: Vec<i64> = before.keys().copied().collect();
    ids.sort();
    let c = conn.lock().unwrap();
    for id in &ids {
        let m = db::get_message(&c, *id).unwrap().unwrap();
        let mut h = DefaultHasher::new();
        m.request_body.as_bytes().hash(&mut h);
        m.response_body.as_bytes().hash(&mut h);
        assert_eq!(
            h.finish(),
            before[id],
            "行 {id} 解码后的请求体/响应体与压缩前不一致"
        );
    }
    let (count, stored): (i64, i64) = c
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(octet_length(request_body)+octet_length(response_body)),0)
             FROM messages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(count as usize, before.len(), "行数不应变化");
    stored as usize
}

fn wait_finished(m: &Arc<maintenance::Maintenance>) -> maintenance::JobResult {
    let started = Instant::now();
    loop {
        match m.snapshot() {
            None => return m.last_result().expect("任务结束后应有结果"),
            Some(v) => {
                if started.elapsed().as_secs() % 30 == 0 {
                    eprintln!(
                        "[real_db] 进行中：{}（已用时 {}s）",
                        v.stage,
                        v.elapsed_ms / 1000
                    );
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn fmt_secs(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

#[test]
#[ignore]
fn real_copy_compress_is_lossless_and_shrinks() {
    let path = target();
    let level = level();

    let t = Instant::now();
    let (before, bytes_before) = hash_raw(&path);
    eprintln!(
        "[real_db] 压缩前：{} 行，body 合计 {}，哈希耗时 {:.1}s",
        before.len(),
        maintenance::fmt_bytes(bytes_before as u64),
        t.elapsed().as_secs_f64()
    );
    let disk_before = disk_len(&path);

    // 迁移（补列 + 部分索引），计时：旧库首启会在窗口出现前走这一步
    let t = Instant::now();
    let conn = Arc::new(Mutex::new(
        db::init_db(path.to_string_lossy().as_ref()).unwrap(),
    ));
    {
        let c = conn.lock().unwrap();
        let s = db::storage_stats(&c).unwrap();
        eprintln!(
            "[real_db] init_db 耗时 {:.1}s：page_size={} page_count={} freelist={} legacy_rows={}",
            t.elapsed().as_secs_f64(),
            s.page_size,
            s.page_count,
            s.freelist_count,
            s.legacy_rows
        );
        assert_eq!(s.legacy_rows as usize, before.len(), "副本应全部为明文行");
    }

    // 回填压缩：与应用完全相同的任务路径
    let m = Arc::new(maintenance::Maintenance::new());
    m.start_compress(Arc::clone(&conn), Arc::new(|| false), level)
        .unwrap();
    let r = wait_finished(&m);
    assert_eq!(r.outcome, Outcome::Completed, "{}", r.message);
    let stored_after = verify_decoded(&conn, &before);
    {
        let c = conn.lock().unwrap();
        let s = db::storage_stats(&c).unwrap();
        let deltas: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE req_ref_id IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        eprintln!(
            "[real_db] 压缩（level {level}）耗时 {}：存储态 body {} → {}，{} 行为增量",
            fmt_secs(r.elapsed_ms),
            maintenance::fmt_bytes(bytes_before as u64),
            maintenance::fmt_bytes(stored_after as u64),
            deltas
        );
        eprintln!(
            "[real_db] 压缩后：文件 {}，可回收 {}，整理后预计 {}",
            maintenance::fmt_bytes(disk_len(&path)),
            maintenance::fmt_bytes(s.reclaimable_bytes() as u64),
            maintenance::fmt_bytes((s.file_bytes() - s.reclaimable_bytes()) as u64)
        );
    }

    // 整理文件
    let m = Arc::new(maintenance::Maintenance::new());
    m.start_vacuum(Arc::clone(&conn)).unwrap();
    let r = wait_finished(&m);
    assert_eq!(r.outcome, Outcome::Completed, "{}", r.message);
    {
        let c = conn.lock().unwrap();
        let check: String = c
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        assert_eq!(db::storage_stats(&c).unwrap().freelist_count, 0);
    }
    verify_decoded(&conn, &before);
    let disk_after = disk_len(&path);
    eprintln!(
        "[real_db] 整理耗时 {}：磁盘占用 {} → {}（{:.1}%）",
        fmt_secs(r.elapsed_ms),
        maintenance::fmt_bytes(disk_before),
        maintenance::fmt_bytes(disk_after),
        disk_after as f64 * 100.0 / disk_before as f64
    );
    assert!(
        disk_after < disk_before / 2,
        "整理后体积应至少减半：{} -> {disk_after}",
        disk_before
    );
}
