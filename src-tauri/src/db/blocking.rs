//! 异步 → 阻塞边界：所有 SQLite 访问都必须经过这里。
//!
//! `rusqlite` 是同步 API；`Db::conn()` 取连接（r2d2 最长等待 5s）与执行查询都会
//! 阻塞线程。直接在 `async fn` 里调用会占住 Tokio worker，导致 IPC 命令与代理
//! 请求一起排队。这里统一用 `spawn_blocking` 把整段闭包挪到阻塞线程池。
//!
//! 纪律：`PooledConnection` 必须在闭包内取、在闭包内用完，不得跨 `.await`；
//! 闭包返回 `Result<T, DbError>`，`T` 需 `Send + 'static`。

use super::{Db, DbError};

/// 在阻塞线程池上执行一段数据库操作。
///
/// 调用方必须把**取连接 + 查询**整体放进闭包：`Db::conn()` 本身可能阻塞。
pub async fn run<T, F>(db: &Db, action: F) -> Result<T, DbError>
where
    F: FnOnce(Db) -> Result<T, DbError> + Send + 'static,
    T: Send + 'static,
{
    let db = db.clone();
    match tokio::task::spawn_blocking(move || action(db)).await {
        Ok(result) => result,
        // 生产配置是 panic="abort"，正常走不到这里；保留显式错误而不是 unwrap。
        Err(err) => Err(DbError::TaskJoin(err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_query_off_the_async_worker() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        let value = run(&db, |db| {
            let conn = db.conn()?;
            let value: i64 = conn.query_row("SELECT 41 + 1", [], |row| row.get(0))?;
            Ok(value)
        })
        .await
        .expect("查询应成功");

        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn propagates_db_errors() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        let error = run(&db, |db| {
            let conn = db.conn()?;
            conn.execute_batch("SELEC broken")?;
            Ok(())
        })
        .await
        .expect_err("语法错误应上抛");

        assert!(matches!(error, DbError::Sqlite(_)));
    }
}
