-- ============================================================================
-- 0003_request_day_status（步骤 1/2）：请求列表的「本地日」落库
--
-- 背景：started_at 存的是 UTC RFC3339，列表用
-- substr(started_at,1,10) 过滤拿到的是 UTC 日；UTC+8 用户「今天」会漏掉
-- 00:00–08:00 的全部请求，而仪表盘「今日」走 usage_daily（本地日），两边数字
-- 对不上。该字段由 Rust 在写入时算好（见 proxy/recorder.rs），本迁移只做结构
-- 补齐与历史行回填；回填用 SQLite 的 localtime 修饰符（按升级当时的机器时区）。
--
-- 拆成两个文件（day_local / status_kind）是为了让 db/mod.rs 逐条语句探测：
-- 任一列已存在都能只跳过那一列，不会撞 duplicate column。
-- ============================================================================

ALTER TABLE proxy_session_requests ADD COLUMN day_local TEXT;

-- 历史行回填：非法时间戳（返回 NULL）退回原来的 UTC 日期前缀，避免整行消失。
UPDATE proxy_session_requests
   SET day_local = COALESCE(date(started_at, 'localtime'), substr(started_at, 1, 10))
 WHERE day_local IS NULL;

-- 0004 会用它替换成匹配 keyset 排序的新索引；这里保留历史形态。
CREATE INDEX IF NOT EXISTS ix_psr_day_local
    ON proxy_session_requests(day_local, started_at DESC);
