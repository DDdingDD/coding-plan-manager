use crate::db::StorageStats;
use crate::maintenance::{JobResult, JobView};
use crate::state::AppState;
use serde::Serialize;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
pub struct DbInfo {
    pub db_path: String,
    pub db_size: u64,
    pub wal_size: u64,
    pub shm_size: u64,
    pub total_size: u64,
    // ---- 页级统计：需要数据库锁；拿不到（整理文件期间锁被 VACUUM 独占）时为 null ----
    pub page_size: Option<i64>,
    pub page_count: Option<i64>,
    pub freelist_count: Option<i64>,
    /// 可回收字节数（空闲页）：回填压缩 / 删除消息腾出，整理文件后归还文件系统
    pub reclaimable_bytes: Option<i64>,
    /// 整理后的预计大小（主库文件减去可回收部分）
    pub estimated_size: Option<i64>,
    /// 仍为明文（未压缩）的消息行数
    pub legacy_rows: Option<i64>,
    // ---- 运行态（不依赖数据库锁）----
    pub running_aggregators: usize,
    /// 进行中的维护任务
    pub job: Option<JobView>,
    /// 最近一次维护任务的结局
    pub last_result: Option<JobResult>,
}

/// 在完整数据库文件名后追加 SQLite 辅助文件后缀（不能替换 .db 扩展名）
fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn file_size(path: &Path, missing_ok: bool) -> Result<u64, String> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(e) if missing_ok && e.kind() == ErrorKind::NotFound => Ok(0),
        Err(e) => Err(format!("读取文件大小失败（{}）：{e}", path.display())),
    }
}

/// 组装配置页的数据库卡片数据。storage 为 None 表示当前拿不到数据库锁
/// （整理文件期间）：页级字段返回 null，文件大小与运行态照常
fn collect_db_info(
    db_path: &Path,
    storage: Option<StorageStats>,
    running_aggregators: usize,
    job: Option<JobView>,
    last_result: Option<JobResult>,
) -> Result<DbInfo, String> {
    let db_size = file_size(db_path, false)?;
    let wal_size = file_size(&sidecar_path(db_path, "-wal"), true)?;
    let shm_size = file_size(&sidecar_path(db_path, "-shm"), true)?;
    let total_size = db_size
        .checked_add(wal_size)
        .and_then(|size| size.checked_add(shm_size))
        .ok_or_else(|| "数据库文件大小合计溢出".to_string())?;

    Ok(DbInfo {
        db_path: db_path.to_string_lossy().into_owned(),
        db_size,
        wal_size,
        shm_size,
        total_size,
        page_size: storage.as_ref().map(|s| s.page_size),
        page_count: storage.as_ref().map(|s| s.page_count),
        freelist_count: storage.as_ref().map(|s| s.freelist_count),
        reclaimable_bytes: storage.as_ref().map(|s| s.reclaimable_bytes()),
        estimated_size: storage.as_ref().map(|s| s.file_bytes() - s.reclaimable_bytes()),
        legacy_rows: storage.as_ref().map(|s| s.legacy_rows),
        running_aggregators,
        job,
        last_result,
    })
}

/// 配置页每秒轮询（有维护任务时）。页级统计用 try_lock：整理文件期间数据库锁被
/// 独占数分钟，不能让轮询跟着挂起——此时仅页级字段为 null，任务进度照常返回
#[tauri::command]
pub async fn get_db_info(state: tauri::State<'_, AppState>) -> Result<DbInfo, String> {
    let storage = match state.db.try_lock() {
        Ok(conn) => Some(crate::db::storage_stats(&conn).map_err(|e| format!("数据库错误: {e}"))?),
        Err(_) => None,
    };
    let running_aggregators = state
        .servers
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    collect_db_info(
        &state.db_path,
        storage,
        running_aggregators,
        state.maintenance.snapshot(),
        state.maintenance.last_result(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let id = NEXT_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("cpm-db-info-{name}-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn db_info_tolerates_missing_sidecars() {
        let dir = temp_dir("main-only");
        let db_path = dir.join("cpm.db");
        fs::write(&db_path, vec![0_u8; 1234]).unwrap();

        let info = collect_db_info(&db_path, None, 0, None, None).unwrap();
        assert_eq!(Path::new(&info.db_path), db_path.as_path());
        assert_eq!(info.db_size, 1234);
        assert_eq!(info.wal_size, 0);
        assert_eq!(info.shm_size, 0);
        assert_eq!(info.total_size, 1234);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn db_info_sums_main_wal_and_shm() {
        let dir = temp_dir("all-files");
        let db_path = dir.join("cpm.db");
        fs::write(&db_path, vec![0_u8; 100]).unwrap();
        fs::write(sidecar_path(&db_path, "-wal"), vec![0_u8; 20]).unwrap();
        fs::write(sidecar_path(&db_path, "-shm"), vec![0_u8; 3]).unwrap();

        let info = collect_db_info(&db_path, None, 0, None, None).unwrap();
        assert_eq!(info.db_size, 100);
        assert_eq!(info.wal_size, 20);
        assert_eq!(info.shm_size, 3);
        assert_eq!(info.total_size, 123);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn db_info_rejects_missing_main_db() {
        let dir = temp_dir("missing-main");
        let db_path = dir.join("cpm.db");

        let error = collect_db_info(&db_path, None, 0, None, None).unwrap_err();
        assert!(error.contains("cpm.db"));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn db_info_storage_stats_optional() {
        let dir = temp_dir("storage");
        let db_path = dir.join("cpm.db");
        fs::write(&db_path, vec![0_u8; 100]).unwrap();
        let stats = StorageStats {
            page_size: 4096,
            page_count: 1000,
            freelist_count: 250,
            legacy_rows: 7,
        };

        // 拿不到数据库锁：页级字段为 null，文件大小与运行态照常
        let info = collect_db_info(&db_path, None, 2, None, None).unwrap();
        assert!(info.page_size.is_none());
        assert!(info.reclaimable_bytes.is_none());
        assert!(info.estimated_size.is_none());
        assert!(info.legacy_rows.is_none());
        assert_eq!((info.db_size, info.running_aggregators), (100, 2));

        let info = collect_db_info(&db_path, Some(stats), 0, None, None).unwrap();
        assert_eq!(info.page_size, Some(4096));
        assert_eq!(info.page_count, Some(1000));
        assert_eq!(info.reclaimable_bytes, Some(4096 * 250));
        assert_eq!(info.estimated_size, Some(4096 * 750));
        assert_eq!(info.legacy_rows, Some(7));

        fs::remove_dir_all(dir).unwrap();
    }
}
