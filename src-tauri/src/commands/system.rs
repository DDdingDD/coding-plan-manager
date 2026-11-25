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

fn collect_db_info(db_path: &Path) -> Result<DbInfo, String> {
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
    })
}

#[tauri::command]
pub async fn get_db_info(state: tauri::State<'_, AppState>) -> Result<DbInfo, String> {
    collect_db_info(&state.db_path)
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

        let info = collect_db_info(&db_path).unwrap();
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

        let info = collect_db_info(&db_path).unwrap();
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

        let error = collect_db_info(&db_path).unwrap_err();
        assert!(error.contains("cpm.db"));

        fs::remove_dir_all(dir).unwrap();
    }
}
