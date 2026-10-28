-- ============================================================================
-- 0003_request_status_kind（步骤 2/2）：请求的「状态分类」落库
--
-- 背景：状态筛选（SQL CASE）与状态标签（Rust 映射）曾
-- 各写一套，导致选「失败」看不到 upstream_error/timeout 这类行，选「已取消」
-- 反而把它们列出来。status_kind 由 usage::aggregate::status_kind 定义并落库，
-- 写入、渲染、筛选共用同一份判定。
-- ============================================================================

ALTER TABLE proxy_session_requests ADD COLUMN status_kind TEXT;

-- 历史行回填：与 usage::aggregate::status_kind 的判定保持一致。
UPDATE proxy_session_requests
   SET status_kind = CASE
       WHEN error_kind IS NOT NULL THEN
         CASE WHEN error_kind LIKE '%disconnect%' OR error_kind LIKE '%cancel%'
                OR error_kind LIKE '%abort%' THEN 'cancelled' ELSE 'error' END
       WHEN status_code BETWEEN 200 AND 399 THEN 'success'
       WHEN status_code IS NOT NULL THEN 'error'
       ELSE 'unknown' END
 WHERE status_kind IS NULL;

-- 0004 会用 (day_local, status_kind, started_at, id) 复合索引替换它。
CREATE INDEX IF NOT EXISTS ix_psr_status_kind
    ON proxy_session_requests(status_kind);

-- 旧索引只覆盖 status_code 单列，对真实筛选条件（status_kind）永不可用。
DROP INDEX IF EXISTS ix_psr_status;
