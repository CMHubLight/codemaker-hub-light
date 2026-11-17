//! 数据库模块：连接池 + 迁移 + 只读查询仓储。

pub mod blocking;
pub mod maintenance;
pub mod pool;
pub mod repo;
pub mod rows;
pub mod sql;

pub use pool::{Db, DbError};

/// 本地数据库文件名（位于 `~/.codemaker-hub-light/`）。
pub const DB_FILE_NAME: &str = "codemaker-hub-light.db";

/// 迁移中的一条语句。
///
/// `probe` 用于 `ALTER TABLE ... ADD COLUMN` 这类**非幂等**语句：探测命中说明结构
/// 已经生效，只跳过这一条，同一迁移的其余语句照常执行。
///
/// 逐语句探测是必须的：0003 曾经有两条 `ALTER TABLE` 却只探一条，若旧版本在
/// "`day_local` 已提交、`status_kind` 未提交"的窗口崩溃过，重跑会撞 duplicate
/// column，而迁移失败会让应用每次启动都失败。
///
/// 幂等语句（`CREATE ... IF NOT EXISTS`、带 `WHERE ... IS NULL` 的回填、去重
/// `DELETE`）不需要 `probe`，重复执行没有副作用。
struct Step {
    sql: &'static str,
    probe: Option<&'static str>,
}

struct Migration {
    version: i64,
    name: &'static str,
    steps: &'static [Step],
}

/// 内嵌全部迁移脚本；新增迁移时只需追加并递增 version。
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "0001_init",
        // 0001 的 DDL 全是 IF NOT EXISTS，天然幂等，单条语句即可。
        steps: &[Step {
            sql: include_str!("../../migrations/0001_init.sql"),
            probe: None,
        }],
    },
    Migration {
        version: 2,
        name: "0002_auth_models",
        steps: &[Step {
            sql: include_str!("../../migrations/0002_auth_models.sql"),
            probe: Some(
                "SELECT 1 FROM pragma_table_info('auth_profiles') WHERE name = 'models_json'",
            ),
        }],
    },
    Migration {
        version: 3,
        name: "0003_request_day_status",
        // 拆成两条语句、各自探测：任一列缺失都能只补那一列。
        steps: &[
            Step {
                sql: include_str!("../../migrations/0003_request_day_status.sql"),
                probe: Some(
                    "SELECT 1 FROM pragma_table_info('proxy_session_requests') \
                     WHERE name = 'day_local'",
                ),
            },
            Step {
                sql: include_str!("../../migrations/0003_request_status_kind.sql"),
                probe: Some(
                    "SELECT 1 FROM pragma_table_info('proxy_session_requests') \
                     WHERE name = 'status_kind'",
                ),
            },
        ],
    },
    Migration {
        version: 4,
        name: "0004_usage_perf",
        // 整批可重复执行（索引 IF NOT EXISTS + 去重 DELETE），所以单条语句 +
        // "最后一条语句建出来的对象"作探测：命中即说明整批已完成。
        steps: &[Step {
            sql: include_str!("../../migrations/0004_usage_perf.sql"),
            probe: Some(
                "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'ux_prc_request'",
            ),
        }],
    },
    Migration {
        version: 5,
        name: "0005_backfill_repair",
        // **刻意不设 probe**：探测只能回答"列在不在"，回答不了"列里还有没有 NULL
        // 行"。0003 在"ALTER 已提交、回填未跑"的窗口崩溃后，重跑时探测命中列、
        // 整条语句被跳过，历史行的 day_local/status_kind 就永久是 NULL。这里用
        // 不带探测的幂等回填（`WHERE ... IS NULL`），版本行缺失就无条件补跑一遍，
        // 重复执行零副作用。
        steps: &[Step {
            sql: include_str!("../../migrations/0005_backfill_repair.sql"),
            probe: None,
        }],
    },
];

/// 最新版本号（迁移列表的最后一个）。
fn latest_version() -> i64 {
    MIGRATIONS.last().map_or(0, |migration| migration.version)
}

/// 应用迁移。
///
/// 每个版本一个事务：该版本的所有语句与版本行同时提交，不会留下
/// "结构已改、版本没记"的中间态。
pub fn run_migrations(db: &Db) -> Result<(), DbError> {
    run_migrations_until(db, None)
}

/// 只应用到 `max_version`（含）；`None` 表示全部。供测试构造"旧版本库"。
fn run_migrations_until(db: &Db, max_version: Option<i64>) -> Result<(), DbError> {
    let target = max_version.map_or_else(latest_version, |max| max.min(latest_version()));
    let mut conn = db.conn()?;

    // 快路径：绝大多数启动的库已是最新版本，一次 MAX(version) 就够，不必逐版本
    // 查 schema_migrations。结构自愈仍走下面的慢路径。
    if applied_version(&conn)?.is_some_and(|applied| applied >= target) {
        return Ok(());
    }

    for migration in MIGRATIONS {
        if migration.version > target {
            break;
        }
        if migration_applied(&conn, migration.version)? {
            continue;
        }

        let tx = conn.transaction()?;
        let mut executed = 0usize;
        for step in migration.steps {
            if step.present(&tx) {
                continue;
            }
            tx.execute_batch(step.sql)
                .map_err(|err| DbError::Migration {
                    version: migration.version,
                    message: err.to_string(),
                })?;
            executed += 1;
        }
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![migration.version, migration.name, repo::now_rfc3339()],
        )?;
        tx.commit()?;
        if executed > 0 {
            tracing::info!(
                version = migration.version,
                name = migration.name,
                statements = executed,
                "数据库迁移已应用"
            );
        }
    }
    Ok(())
}

impl Step {
    /// 结构是否已经生效（探测命中即跳过该语句）。
    ///
    /// 探测 SQL 自身出错（列/表不存在等）按"未生效"处理，由随后的正式 DDL 报出
    /// 真实错误——与旧实现一致，避免把探测失败误判成"已生效"而静默漏建。
    fn present(&self, conn: &rusqlite::Connection) -> bool {
        match self.probe {
            Some(sql) => conn.query_row(sql, [], |_| Ok(())).is_ok(),
            None => false,
        }
    }
}

/// 当前已应用的最大版本；`schema_migrations` 还不存在时返回 `None`。
fn applied_version(conn: &rusqlite::Connection) -> Result<Option<i64>, DbError> {
    if !schema_migrations_exists(conn)? {
        return Ok(None);
    }
    let version: Option<i64> =
        conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })?;
    Ok(version)
}

fn schema_migrations_exists(conn: &rusqlite::Connection) -> Result<bool, DbError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations')",
        [],
        |row| row.get(0),
    )?)
}

/// 某个版本的版本行是否已存在（`schema_migrations` 由 0001 创建）。
fn migration_applied(conn: &rusqlite::Connection, version: i64) -> Result<bool, DbError> {
    if !schema_migrations_exists(conn)? {
        return Ok(false);
    }
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = ?1)",
        [version],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_create_all_tables() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        run_migrations(&db).expect("迁移应成功");

        let conn = db.conn().expect("应能取连接");
        let expected = [
            "auth_profiles",
            "proxy_sessions",
            "proxy_session_requests",
            "proxy_request_costs",
            "usage_daily",
            "quota_snapshots",
            "model_prices",
            "schema_migrations",
        ];
        for table in expected {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                    [table],
                    |row| row.get(0),
                )
                .expect("查询表是否存在应成功");
            assert!(exists, "缺少表 {table}");
        }

        // 0002 追加的模型缓存列。
        let models_column: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('auth_profiles') WHERE name = 'models_json')",
                [],
                |row| row.get(0),
            )
            .expect("查询列是否存在应成功");
        assert!(models_column, "auth_profiles 缺少 models_json 列");

        // 0003 追加的本地日 / 状态列。
        for column in ["day_local", "status_kind"] {
            let present: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('proxy_session_requests') WHERE name = ?1)",
                    [column],
                    |row| row.get(0),
                )
                .expect("查询列是否存在应成功");
            assert!(present, "proxy_session_requests 缺少 {column} 列");
        }

        let version = repo::schema_version(&conn).expect("读取版本应成功");
        assert_eq!(version, latest_version());
    }

    #[test]
    fn migrations_are_idempotent() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        run_migrations(&db).expect("首次迁移应成功");
        run_migrations(&db).expect("重复迁移应成功");
        let conn = db.conn().expect("应能取连接");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .expect("计数应成功");
        assert_eq!(count, MIGRATIONS.len() as i64);
    }

    /// 模拟"0002 的 DDL 已提交、版本行还没写入"时崩溃，
    /// 下次启动重跑迁移不能因为 duplicate column 而永久启动失败。
    #[test]
    fn migrations_retry_after_partial_0002() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        run_migrations_until(&db, Some(2)).expect("首次迁移应成功");
        {
            let conn = db.conn().expect("应能取连接");
            conn.execute("DELETE FROM schema_migrations WHERE version = 2", [])
                .expect("删除版本行应成功");
        }

        run_migrations_until(&db, Some(2)).expect("结构已在时重跑迁移必须成功（只补版本行）");

        let conn = db.conn().expect("应能取连接");
        assert_eq!(repo::schema_version(&conn).expect("读取版本应成功"), 2);
    }

    /// 结构真的缺失时（探测 SQL 未命中）要重新执行 0002，而不是跳过。
    #[test]
    fn migrations_reapply_when_column_missing() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        run_migrations_until(&db, Some(2)).expect("首次迁移应成功");
        {
            let conn = db.conn().expect("应能取连接");
            conn.execute_batch(
                "ALTER TABLE auth_profiles DROP COLUMN models_json; \
                 DELETE FROM schema_migrations WHERE version = 2;",
            )
            .expect("撤回 0002 应成功");
        }

        run_migrations_until(&db, Some(2)).expect("列缺失时应重新应用 0002");

        let conn = db.conn().expect("应能取连接");
        let present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('auth_profiles') WHERE name = 'models_json')",
                [],
                |row| row.get(0),
            )
            .expect("查询列应成功");
        assert!(present, "models_json 应被重新添加");
    }

    /// 0003 与 0002 同构：版本行缺失但结构已在时，重跑只补版本行。
    #[test]
    fn migrations_retry_after_partial_0003() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        run_migrations(&db).expect("首次迁移应成功");
        {
            let conn = db.conn().expect("应能取连接");
            // 同时删掉 3/4 的版本行，才会走慢路径（否则快路径直接返回）。
            conn.execute("DELETE FROM schema_migrations WHERE version >= 3", [])
                .expect("删除版本行应成功");
        }

        run_migrations(&db).expect("结构已在时重跑 0003/0004 必须成功");

        let conn = db.conn().expect("应能取连接");
        assert_eq!(
            repo::schema_version(&conn).expect("读取版本应成功"),
            latest_version()
        );
    }

    /// 0003 的两条 ALTER 现在各自探测：只有 `day_local` 缺失（崩溃窗口里
    /// `status_kind` 已提交）时，重跑必须只补 `day_local` 并回填，而不是撞
    /// duplicate column 让应用永久启动失败。
    #[test]
    fn migration_0003_partial_retry_heals_day_local_only() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        run_migrations(&db).expect("首次迁移应成功");
        {
            let conn = db.conn().expect("应能取连接");
            // 造一条旧行：回退后 day_local 会丢，只能靠回填恢复。
            conn.execute(
                "INSERT INTO proxy_session_requests \
                   (method, request_path, api_format, status_code, started_at, ended_at) \
                 VALUES ('POST', '/v1/messages', 'anthropic', 200, \
                         '2026-10-21T16:30:00Z', '2026-10-21T16:30:01Z')",
                [],
            )
            .expect("插入旧行应成功");
            // 引用 day_local 的索引必须先删，SQLite 才允许 DROP COLUMN。
            conn.execute_batch(
                "DROP INDEX IF EXISTS ix_psr_day_local; \
                 DROP INDEX IF EXISTS ix_psr_day_started_id; \
                 DROP INDEX IF EXISTS ix_psr_status_day_started; \
                 DROP INDEX IF EXISTS ix_psr_format_day_started; \
                 DROP INDEX IF EXISTS ux_prc_request; \
                 ALTER TABLE proxy_session_requests DROP COLUMN day_local; \
                 DELETE FROM schema_migrations WHERE version >= 3;",
            )
            .expect("回退 day_local 应成功");
        }

        run_migrations(&db).expect("仅 day_local 缺失时重跑迁移必须成功");

        let conn = db.conn().expect("应能取连接");
        assert_eq!(
            repo::schema_version(&conn).expect("读取版本应成功"),
            latest_version()
        );
        for column in ["day_local", "status_kind"] {
            let present: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('proxy_session_requests') \
                     WHERE name = ?1)",
                    [column],
                    |row| row.get(0),
                )
                .expect("查询列应成功");
            assert!(present, "重跑后仍缺列 {column}");
        }

        let started_at = chrono::DateTime::parse_from_rfc3339("2026-10-21T16:30:00Z")
            .expect("时间戳应可解析")
            .with_timezone(&chrono::Utc);
        let day_local: String = conn
            .query_row("SELECT day_local FROM proxy_session_requests", [], |row| {
                row.get(0)
            })
            .expect("回填后的 day_local 应可读");
        assert_eq!(
            day_local,
            crate::usage::local_day(started_at),
            "重跑时必须回填 day_local"
        );

        let index_rebuilt: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' \
                 AND name = 'ix_psr_day_started_id')",
                [],
                |row| row.get(0),
            )
            .expect("查询索引应成功");
        assert!(index_rebuilt, "0004 的 keyset 索引应随重跑重建");
    }

    /// 0004 的数据语句（去重）与 DDL（唯一索引）必须可整批重跑：先回退成
    /// "未建唯一索引 + 已存在重复费用行"，重跑后保留 MAX(id) 并重建唯一索引。
    ///
    /// 版本行必须从 4 起**全删**（含之后新增的 5…）：`run_migrations` 的快路径用
    /// `MAX(version)` 判断"是否已是最新"，只删 4 而留下 5 会直接走快路径返回，
    /// 0004 根本不会重跑。
    #[test]
    fn migration_0004_partial_retry_heals() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        run_migrations(&db).expect("首次迁移应成功");
        {
            let conn = db.conn().expect("应能取连接");
            conn.execute_batch(
                "DROP INDEX IF EXISTS ux_prc_request; \
                 DROP INDEX IF EXISTS ix_quota_snapshots_ok_latest; \
                 DROP INDEX IF EXISTS ix_psr_day_started_id; \
                 DROP INDEX IF EXISTS ix_psr_status_day_started; \
                 DELETE FROM schema_migrations WHERE version >= 4;",
            )
            .expect("回退 0004 应成功");
            conn.execute_batch(
                "INSERT INTO proxy_session_requests \
                   (method, request_path, api_format, started_at, day_local, status_kind) \
                 VALUES ('POST', '/v1/messages', 'anthropic', '2026-10-20T00:00:00Z', \
                         '2026-10-20', 'success');",
            )
            .expect("插入请求行应成功");
            // 唯一索引缺失时才可能出现的重复费用行（旧代码按 1:1 假设，未加约束）。
            conn.execute_batch(
                "INSERT INTO proxy_request_costs (request_id, cost_yuan, created_at) \
                   SELECT id, 1.0, '2026-10-20T00:00:00Z' FROM proxy_session_requests; \
                 INSERT INTO proxy_request_costs (request_id, cost_yuan, created_at) \
                   SELECT id, 2.0, '2026-10-20T00:00:01Z' FROM proxy_session_requests;",
            )
            .expect("插入重复费用行应成功");
        }

        run_migrations(&db).expect("结构回退后重跑 0004 必须成功");

        let conn = db.conn().expect("应能取连接");
        assert_eq!(
            repo::schema_version(&conn).expect("读取版本应成功"),
            latest_version()
        );

        let (count, latest_cost): (i64, f64) = conn
            .query_row(
                "SELECT COUNT(*), MAX(cost_yuan) FROM proxy_request_costs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("统计费用行应成功");
        assert_eq!(count, 1, "重跑 0004 必须去掉重复费用行");
        assert_eq!(latest_cost, 2.0, "去重保留 MAX(id)（较新的一条）");

        for index in [
            "ux_prc_request",
            "ix_quota_snapshots_ok_latest",
            "ix_psr_day_started_id",
        ] {
            let present: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)",
                    [index],
                    |row| row.get(0),
                )
                .expect("查询索引应成功");
            assert!(present, "重跑 0004 后缺少索引 {index}");
        }
    }

    /// 0003 必须把旧行（UTC 时间戳）回填成按机器本地时区换算的 day_local，
    /// 并给出与 usage::aggregate::status_kind 一致的状态分类。
    #[test]
    fn migration_0003_backfills_legacy_rows() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        // 造一个 0002 时代的库，并写入一条"旧结构"请求（此时还没有新列）。
        run_migrations_until(&db, Some(2)).expect("0002 应可应用");
        {
            let conn = db.conn().expect("应能取连接");
            conn.execute(
                "INSERT INTO proxy_session_requests \
                   (method, request_path, api_format, status_code, started_at, ended_at) \
                 VALUES ('POST', '/v1/messages', 'anthropic', 200, \
                         '2026-10-21T16:30:00Z', '2026-10-21T16:30:01Z')",
                [],
            )
            .expect("插入旧行应成功");
        }

        run_migrations(&db).expect("0003 应可应用");

        let started_at = chrono::DateTime::parse_from_rfc3339("2026-10-21T16:30:00Z")
            .expect("时间戳应可解析")
            .with_timezone(&chrono::Utc);
        let expected_day = crate::usage::local_day(started_at);

        let conn = db.conn().expect("应能取连接");
        let (day_local, status_kind): (String, String) = conn
            .query_row(
                "SELECT day_local, status_kind FROM proxy_session_requests",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("回填后的列应可读");
        assert_eq!(day_local, expected_day, "day_local 应按本地时区回填");
        assert_eq!(status_kind, "success");
    }

    /// 直接插一行"0003 回填被跳过"的历史行：两列都是 NULL。
    fn insert_legacy_row(
        conn: &rusqlite::Connection,
        started_at: &str,
        status_code: Option<i64>,
        error_kind: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO proxy_session_requests \
               (method, request_path, api_format, status_code, error_kind, started_at) \
             VALUES ('POST', '/v1/messages', 'anthropic', ?1, ?2, ?3)",
            rusqlite::params![status_code, error_kind, started_at],
        )
        .expect("插入历史行应成功");
    }

    fn null_backfill_counts(conn: &rusqlite::Connection) -> (i64, i64) {
        conn.query_row(
            "SELECT SUM(day_local IS NULL), SUM(status_kind IS NULL) \
             FROM proxy_session_requests",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("统计 NULL 行应成功")
    }

    /// 0005：版本 4 的库里存在 `day_local`/`status_kind` 为 NULL 的历史行（0003 的
    /// 回填在崩溃窗口里被跳过），`run_migrations` 必须补齐；已有值的行不得被改写；
    /// 再次运行零变更。
    #[test]
    fn migration_0005_backfills_null_columns() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        run_migrations_until(&db, Some(4)).expect("0004 应可应用");
        {
            let conn = db.conn().expect("应能取连接");
            // 覆盖回填 CASE 的四个分支。
            insert_legacy_row(&conn, "2026-10-21T16:30:00Z", Some(200), None);
            insert_legacy_row(
                &conn,
                "2026-10-22T01:00:00Z",
                Some(502),
                Some("upstream_error"),
            );
            insert_legacy_row(&conn, "2026-10-23T02:00:00Z", None, Some("client_aborted"));
            insert_legacy_row(&conn, "2026-10-24T03:00:00Z", None, None);
            // 已回填过的行：0005 不得覆盖（回填语句带 WHERE ... IS NULL）。
            conn.execute(
                "INSERT INTO proxy_session_requests \
                   (method, request_path, api_format, status_code, started_at, day_local, status_kind) \
                 VALUES ('POST', '/v1/messages', 'anthropic', 200, '2026-10-20T00:00:00Z', \
                         '2026-10-19', 'cancelled')",
                [],
            )
            .expect("插入已回填行应成功");
        }

        run_migrations(&db).expect("0005 应可应用");

        let conn = db.conn().expect("应能取连接");
        assert_eq!(
            null_backfill_counts(&conn),
            (0, 0),
            "0005 后不得再有 day_local/status_kind 为 NULL 的行"
        );
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT day_local, status_kind FROM proxy_session_requests ORDER BY id ASC")
            .expect("准备查询应成功")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("查询应成功")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("收集行应成功");
        // day_local 的口径是"本地日"（与 usage::local_day / 0003 一致），
        // 因此期望值按运行测试机器的时区计算，不能硬编码 UTC 日期。
        let expected_day = |started_at: &str| {
            crate::usage::local_day(
                started_at
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .expect("时间应可解析"),
            )
        };
        assert_eq!(
            rows,
            vec![
                (expected_day("2026-10-21T16:30:00Z"), "success".to_string()),
                (expected_day("2026-10-22T01:00:00Z"), "error".to_string()),
                (
                    expected_day("2026-10-23T02:00:00Z"),
                    "cancelled".to_string()
                ),
                (expected_day("2026-10-24T03:00:00Z"), "unknown".to_string()),
                // 已有值原样保留。
                ("2026-10-19".to_string(), "cancelled".to_string()),
            ],
            "回填口径必须与 usage::local_day 一致"
        );
        assert_eq!(
            repo::schema_version(&conn).expect("读取版本应成功"),
            latest_version()
        );
        drop(conn);

        // 再次运行：快路径直接返回，任何行都不许被改动。
        let snapshot = |db: &Db| -> Vec<(i64, Option<String>, Option<String>)> {
            let conn = db.conn().expect("应能取连接");
            let mut stmt = conn
                .prepare(
                    "SELECT id, day_local, status_kind FROM proxy_session_requests ORDER BY id",
                )
                .expect("准备查询应成功");
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .expect("查询应成功")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("收集行应成功");
            rows
        };
        let before = snapshot(&db);
        run_migrations(&db).expect("再次迁移应成功");
        assert_eq!(snapshot(&db), before, "重复运行 0005 不得有任何副作用");
        let version_rows: i64 = {
            let conn = db.conn().expect("应能取连接");
            conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .expect("计数应成功")
        };
        assert_eq!(version_rows, MIGRATIONS.len() as i64);
    }

    /// 0005 不设 probe 的理由：即使 `day_local`/`status_kind` **列已存在**，只要
    /// 版本行缺失（0003 崩溃窗口 / 手工删版本行），重跑也必须补做回填。
    #[test]
    fn migration_0005_rebackfills_when_version_row_missing() {
        let db = Db::open_in_memory().expect("内存库应可打开");
        run_migrations(&db).expect("首次迁移应成功");
        insert_legacy_row(
            &db.conn().expect("应能取连接"),
            "2026-10-25T01:00:00Z",
            Some(500),
            None,
        );
        {
            let conn = db.conn().expect("应能取连接");
            conn.execute_batch("DELETE FROM schema_migrations WHERE version = 5;")
                .expect("删版本行应成功");
        }

        run_migrations(&db).expect("版本行缺失时应重跑 0005");

        let conn = db.conn().expect("应能取连接");
        let (day_local, status_kind): (String, String) = conn
            .query_row(
                "SELECT day_local, status_kind FROM proxy_session_requests",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("回填后的列应可读");
        assert_eq!(day_local, "2026-10-25");
        assert_eq!(status_kind, "error");
    }
}
