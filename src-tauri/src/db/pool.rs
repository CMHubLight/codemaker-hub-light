//! SQLite 连接池。
//!
//! 使用 r2d2 + r2d2_sqlite；每个连接初始化时统一打开 WAL / 外键 / busy_timeout，
//! 保证与 `migrations/0001_init.sql` 顶部的 PRAGMA 一致。

use std::{path::Path, time::Duration};

use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

/// 每个连接建立时执行的 PRAGMA。
pub const CONNECTION_PRAGMAS: &str = "PRAGMA journal_mode = WAL; \
     PRAGMA foreign_keys = ON; \
     PRAGMA synchronous = NORMAL; \
     PRAGMA busy_timeout = 5000;";

const POOL_MAX_SIZE: u32 = 8;
const POOL_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// 内存库允许的连接数。
///
/// 必须 > 1：测试里同时持有两条连接是常见写法，而 r2d2_sqlite 的
/// `memory()` 是"每个连接新开一个空库"，第二条连接会看不到任何表（静默丢库）。
/// 这里改用 shared-cache 命名内存库（见 [`Db::open_in_memory`]）。
const MEMORY_POOL_MAX_SIZE: u32 = 4;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("SQLite 错误：{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("数据库连接池错误：{0}")]
    Pool(#[from] r2d2::Error),
    #[error("数据目录操作失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("迁移 {version} 执行失败：{message}")]
    Migration { version: i64, message: String },
    #[error("参数无效：{0}")]
    InvalidArgument(String),
    #[error("数据库阻塞任务执行失败：{0}")]
    TaskJoin(String),
    #[error("尚未实现：{0}")]
    NotImplemented(&'static str),
}

/// 轻量连接池封装（可 Clone，内部共享池）。
#[derive(Clone)]
pub struct Db {
    pool: r2d2::Pool<SqliteConnectionManager>,
}

impl Db {
    /// 打开（或创建）数据库文件，并建立连接池。
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let manager = SqliteConnectionManager::file(path)
            .with_init(|conn: &mut Connection| conn.execute_batch(CONNECTION_PRAGMAS));
        let pool = r2d2::Pool::builder()
            .max_size(POOL_MAX_SIZE)
            .min_idle(Some(1))
            .connection_timeout(POOL_CONNECTION_TIMEOUT)
            .build(manager)?;
        Ok(Self { pool })
    }

    /// 纯内存库（单元测试 / 冒烟验证用）。
    ///
    /// 用 `file:memdb-<uuid>?mode=memory&cache=shared` 而不是
    /// `SqliteConnectionManager::memory()`：后者每条连接各开一个独立空库，
    /// 池里出现第二条连接就会"丢表"。命名 + shared cache 让池内所有连接指向
    /// 同一个内存库；`min_idle(1)` 保证始终有一条连接存活（shared-cache 内存库
    /// 在最后一条连接关闭时销毁）。uuid 前缀让并行测试互不干扰。
    pub fn open_in_memory() -> Result<Self, DbError> {
        let uri = format!(
            "file:memdb-{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        let manager = SqliteConnectionManager::file(uri.as_str())
            .with_init(|conn: &mut Connection| conn.execute_batch(CONNECTION_PRAGMAS));
        let pool = r2d2::Pool::builder()
            .max_size(MEMORY_POOL_MAX_SIZE)
            .min_idle(Some(1))
            .connection_timeout(POOL_CONNECTION_TIMEOUT)
            .build(manager)
            .map_err(DbError::Pool)?;
        Ok(Self { pool })
    }

    /// 从池中取一个连接。
    ///
    /// 注意：取连接本身可能阻塞（r2d2 最长等待 [`POOL_CONNECTION_TIMEOUT`]），
    /// 因此在 `async fn` 里**必须先经过 `db::blocking::run`**，不要直接调用。
    pub fn conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>, DbError> {
        Ok(self.pool.get()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_in_memory() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        let conn = db.conn().expect("应能取连接");
        let value: i64 = conn
            .query_row("SELECT 1", [], |row| row.get(0))
            .expect("SELECT 1 应成功");
        assert_eq!(value, 1);
    }

    /// 两条连接必须看到同一个内存库（旧实现里第二条连接是一个新空库）。
    #[test]
    fn in_memory_pool_shares_one_database() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        {
            let writer = db.conn().expect("应能取第一条连接");
            writer
                .execute_batch("CREATE TABLE probe (id INTEGER PRIMARY KEY, name TEXT);")
                .expect("建表应成功");
            writer
                .execute("INSERT INTO probe (name) VALUES ('shared')", [])
                .expect("写入应成功");

            // 同时持有第二条连接：旧实现会拿到另一个空库。
            let reader = db.conn().expect("应能取第二条连接");
            let name: String = reader
                .query_row("SELECT name FROM probe", [], |row| row.get(0))
                .expect("第二条连接必须看到同一条数据");
            assert_eq!(name, "shared");
        }
    }

    /// 池内连接全部归还后，内存库仍然存在（min_idle 保持一条连接存活）。
    #[test]
    fn in_memory_database_survives_connection_returns() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        {
            let conn = db.conn().expect("应能取连接");
            conn.execute_batch("CREATE TABLE probe (id INTEGER PRIMARY KEY);")
                .expect("建表应成功");
        }
        // 连接已归还；重新取一条仍应看到表。
        let conn = db.conn().expect("应能取连接");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM probe", [], |row| row.get(0))
            .expect("表应仍然存在");
        assert_eq!(count, 0);
    }
}
