-- ============================================================================
-- 0005_backfill_repair：历史行的 day_local / status_kind 兜底回填
--
-- 背景：0003 的回填只在**升级那一刻**执行一次，且 0003 自身在 `db/mod.rs` 里带
-- 探测 SQL（"列已存在就跳过整条语句"）。于是存在两个漏填窗口：
--   1) 应用在"0003 的 ALTER 已提交、回填还没跑"时崩溃，重启后探测命中
--      `day_local` / `status_kind` 列，整条语句被跳过 → 历史行永久为 NULL；
--   2) 任何绕过 recorder 写入的行（手工修库、旧版本导入、并发极端路径）也可能
--      带着 NULL 落库。
-- NULL 的 day_local 会让请求列表按日区间的查询整行消失（`day_local >= ?` 对
-- NULL 恒假）；NULL 的 status_kind 会让状态筛选漏行。所以这里做一次独立、
-- 幂等的兜底回填。
--
-- 本迁移**故意不设 probe**（`Step { probe: None }`）：探测只能看到列是否存在，
-- 看不到"列里是否还有 NULL 行"。列已存在但版本行缺失时（崩溃窗口）也必须把
-- 回填跑一遍；两条语句都带 `WHERE ... IS NULL`，重复执行没有任何副作用。
-- ============================================================================

-- 1) 本地日回填（修复 0003 回填被跳过的行）。
--    口径必须与 0003 / `usage::local_day` 一致：`date(started_at,'localtime')`，
--    非法时间戳退回 UTC 日期前缀（与 0003 相同），避免历史行消失。
UPDATE proxy_session_requests
   SET day_local = COALESCE(
         date(started_at, 'localtime'),
         date(started_at),
         substr(started_at, 1, 10)
       )
 WHERE day_local IS NULL;

-- 2) 状态分类回填：CASE 与 `migrations/0003_request_status_kind.sql:13-21`
--    （以及 `usage::aggregate::status_kind`）逐字一致，改任意一处必须同步。
UPDATE proxy_session_requests
   SET status_kind = CASE
       WHEN error_kind IS NOT NULL THEN
         CASE WHEN error_kind LIKE '%disconnect%' OR error_kind LIKE '%cancel%'
                OR error_kind LIKE '%abort%' THEN 'cancelled' ELSE 'error' END
       WHEN status_code BETWEEN 200 AND 399 THEN 'success'
       WHEN status_code IS NOT NULL THEN 'error'
       ELSE 'unknown' END
 WHERE status_kind IS NULL;
