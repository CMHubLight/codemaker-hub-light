-- ============================================================================
-- 0004_usage_perf：请求列表 keyset 索引 + 额度快照索引 + usage_daily 覆盖索引
--
-- 依据：请求列表与额度查询的实际执行计划（索引不可用 / 只增不删）。
--   * ix_quota_snapshots_latest 以 profile_id 打头，而真实查询不带 profile_id
--     → SCAN quota_snapshots + 临时 B 树；
--   * 列表固定按 started_at DESC 排序，而旧索引以 day_local 打头且未含 id
--     → 跨日区间必做临时 B 树排序；
--   * 一条请求可能有多行费用（无 UNIQUE），LEFT JOIN 会重复列表项；
--   * (?N IS NULL OR col = ?N) 的可选过滤让等值条件无法走索引。
--
-- 本文件必须保持**整批可重复执行**：若在"索引已建、版本行未写"的窗口崩溃，
-- db/mod.rs 会把整批重跑一次（探测 SQL 指向最后一条语句建出的 ux_prc_request）。
-- 因此所有 DDL 都用 IF NOT EXISTS / IF EXISTS，数据语句（去重 DELETE）可重复执行。
-- ============================================================================

-- 1) 额度快照：精确匹配 `WHERE quota_type = ?1 AND status = 'ok'
--    ORDER BY fetched_at DESC, id DESC LIMIT 1`（db/repo 查询）。
--    部分索引只收录成功快照，体积最小且命中即得。
CREATE INDEX IF NOT EXISTS ix_quota_snapshots_ok_latest
    ON quota_snapshots(quota_type, fetched_at DESC, id DESC) WHERE status = 'ok';

-- 1b) 额度快照剪枝用（按 quota_type 保留最近 N 条 → DELETE ... WHERE id <= ?）。
CREATE INDEX IF NOT EXISTS ix_quota_snapshots_type_id
    ON quota_snapshots(quota_type, id);

-- 2) 请求列表 keyset 主索引：ORDER BY day_local DESC, started_at DESC, id DESC。
--    day_local 是 started_at 的单调函数，以它打头与直接按 started_at 排序等价，
--    但让"日区间 + 全局倒序"变成一次索引区间反向扫描，不再需要临时 B 树。
CREATE INDEX IF NOT EXISTS ix_psr_day_started_id
    ON proxy_session_requests(day_local DESC, started_at DESC, id DESC);

-- 2b) 等值筛选（状态 / 协议）走"等值列在前"的复合索引：等值列打头才能让
--     后面的 day_local 区间 + started_at 倒序同时由索引满足；反过来
--     (day_local, status_kind, ...) 只能区间扫描后逐行比较，等于没用。
CREATE INDEX IF NOT EXISTS ix_psr_status_day_started
    ON proxy_session_requests(status_kind, day_local DESC, started_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS ix_psr_format_day_started
    ON proxy_session_requests(api_format, day_local DESC, started_at DESC, id DESC);

-- 2c) 旧索引已被上面的索引前缀覆盖，保留只增加每次插入的写放大。
DROP INDEX IF EXISTS ix_psr_day_local;
DROP INDEX IF EXISTS ix_psr_status_kind;

-- 3) usage_daily 日区间 SUM 的覆盖索引：SQLite 没有 INCLUDE，只能把聚合列放进键，
--    让仪表盘/趋势查询变成 index-only scan（不回表）。
CREATE INDEX IF NOT EXISTS ix_usage_daily_day_cover
    ON usage_daily(day, request_count, error_count, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens, cost_yuan, total_duration_ms);

-- 4) 一条请求只应有一行费用（代码按 1:1 LEFT JOIN）。旧库可能已经存在重复行：
--    先去重（保留 MAX(id)，即最新写入的那条），再补唯一索引。
--    两条语句都可重复执行：无重复行时 DELETE 影响 0 行。
DELETE FROM proxy_request_costs
 WHERE request_id IS NOT NULL
   AND id NOT IN (
       SELECT MAX(id) FROM proxy_request_costs
        WHERE request_id IS NOT NULL
        GROUP BY request_id
   );

CREATE UNIQUE INDEX IF NOT EXISTS ux_prc_request
    ON proxy_request_costs(request_id);
