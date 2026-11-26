"""把真实库复制到工作目录（只读打开源库，绝不改动它）。

用法（在仓库根目录执行）：
  python scripts/copy-db.py full <src> <dst>              # sqlite3 backup API 整库一致快照
  python scripts/copy-db.py tail <src> <dst> <keep_rows>  # 只取最近 keep_rows 条消息 + 全部配置表

副本用于验证迁移 / 压缩 / 整理（配合 src-tauri/tests/real_db.rs 或 CPM_DB_PATH 调试），
不触碰 %APPDATA% 下的真实库。
"""
import os
import sqlite3
import sys
import time


def human(n: float) -> str:
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} TB"


def open_ro(path: str) -> sqlite3.Connection:
    # mode=ro：不改源库；WAL 模式下读的是含未 checkpoint 帧的一致快照
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def copy_full(src_path: str, dst_path: str) -> None:
    for suffix in ("", "-wal", "-shm"):
        try:
            os.remove(dst_path + suffix)
        except FileNotFoundError:
            pass
    src = open_ro(src_path)
    dst = sqlite3.connect(dst_path)
    t0 = time.time()

    last = [0.0]

    def progress(status, remaining, total):
        now = time.time()
        if now - last[0] >= 10:
            last[0] = now
            done = total - remaining
            print(f"  {human(done * 4096)} / {human(total * 4096)}", flush=True)

    src.backup(dst, progress=progress)
    dst.commit()
    dst.close()
    src.close()
    size = os.path.getsize(dst_path)
    print(f"full copy done in {time.time() - t0:.1f}s -> {dst_path} ({human(size)})", flush=True)


def copy_tail(src_path: str, dst_path: str, keep_rows: int) -> None:
    for suffix in ("", "-wal", "-shm"):
        try:
            os.remove(dst_path + suffix)
        except FileNotFoundError:
            pass
    src = open_ro(src_path)
    dst = sqlite3.connect(dst_path)
    t0 = time.time()

    # 同构 schema：表与索引的 DDL 原样照搬（sqlite_master 的 sql 列即建表语句；
    # sqlite_sequence 由 AUTOINCREMENT 表自动创建，不能手工建）
    for (typ, name, sql) in src.execute(
        "SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type DESC"
    ).fetchall():
        if not name.startswith("sqlite_"):
            dst.execute(sql)
    # 自增序列对齐，副本里新建的行 id 与源库不冲突
    with dst:
        for (name, seq) in src.execute("SELECT name, seq FROM sqlite_sequence").fetchall():
            dst.execute("INSERT INTO sqlite_sequence(name, seq) VALUES (?, ?)", (name, seq))

    with dst:
        for table in ("coding_plans", "aggregators", "aggregator_plans", "settings"):
            rows = src.execute(f"SELECT * FROM {table}").fetchall()
            if rows:
                placeholders = ",".join("?" * len(rows[0]))
                dst.executemany(f"INSERT INTO {table} VALUES ({placeholders})", rows)
        (max_id,) = src.execute("SELECT MAX(id) FROM messages").fetchone()
        since = (max_id or 0) - keep_rows
        rows = src.execute("SELECT * FROM messages WHERE id > ? ORDER BY id", (since,)).fetchall()
        placeholders = ",".join("?" * len(rows[0]))
        dst.executemany(f"INSERT INTO messages VALUES ({placeholders})", rows)
        print(f"copied {len(rows)} messages with id > {since}", flush=True)
    # 源库 checkpoint 后的页里可能有空洞；副本 VACUUM 一下，体积才有参考意义
    dst.execute("VACUUM")
    dst.close()
    src.close()
    size = os.path.getsize(dst_path)
    print(f"tail copy done in {time.time() - t0:.1f}s -> {dst_path} ({human(size)})", flush=True)


if __name__ == "__main__":
    mode = sys.argv[1]
    if mode == "full":
        copy_full(sys.argv[2], sys.argv[3])
    elif mode == "tail":
        copy_tail(sys.argv[2], sys.argv[3], int(sys.argv[4]))
    else:
        sys.exit(__doc__)
