-- ============================================================================
-- CodeMaker Hub Light —— 本地 SQLite 初始化（0001_init）
-- 依据：docs/data-model.md、docs/gateway-contract.md
-- 8 张业务表：auth_profiles / proxy_sessions / proxy_session_requests /
--             proxy_request_costs / usage_daily / quota_snapshots / model_prices /
--             schema_migrations
-- 安全约定：任何 token / auth_key 都不入库；正文默认不入库（recordBodies=false），
--           开启后单侧最多 64KB。
-- 时间约定：业务时间戳统一存 RFC3339 文本（UTC，秒精度）；auth 过期时间用 Unix 秒。
-- 金额约定：cost_yuan 为人民币元（aigw.cost.total_credit / 100），REAL。
-- ============================================================================

-- 连接级 PRAGMA（WAL / foreign_keys / synchronous / busy_timeout）统一由
-- src/db/pool.rs 的 CONNECTION_PRAGMAS 在每个连接建立时设置。
-- 这里不能再写：迁移在事务中执行，而 `PRAGMA journal_mode = WAL` 不允许在事务内切换。

-- ---------------------------------------------------------------------------
-- 迁移版本记录
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS schema_migrations (
    version     INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL,
    applied_at  TEXT    NOT NULL
);

-- ---------------------------------------------------------------------------
-- 账号档案：只存非敏感身份信息；token 在 ~/.codemaker-hub-light/auth.bin（P1）
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS auth_profiles (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    user            TEXT    NOT NULL,                       -- 工号/用户名
    display_name    TEXT,
    dept            TEXT,
    server_url      TEXT    NOT NULL DEFAULT '',            -- 网关地址
    auth_user       TEXT,                                   -- X-Auth-User
    is_active       INTEGER NOT NULL DEFAULT 0,
    issued_at       INTEGER,                                -- Unix 秒
    expires_at      INTEGER,                                -- Unix 秒
    validated_at    INTEGER,                                -- 最近一次 /validate 成功（Unix 秒）
    last_error      TEXT,
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_auth_profiles_user_server
    ON auth_profiles(user, server_url);

CREATE INDEX IF NOT EXISTS ix_auth_profiles_active
    ON auth_profiles(is_active, updated_at DESC);

-- ---------------------------------------------------------------------------
-- 代理会话：按 external_session_id + 协议聚合（upsert）
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS proxy_sessions (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    external_session_id   TEXT,
    api_format            TEXT    NOT NULL DEFAULT 'unknown', -- anthropic|openai_chat|openai_responses
    agent_name            TEXT,
    client                TEXT,                               -- downstream-client
    model                 TEXT,
    project_dir           TEXT,
    request_count         INTEGER NOT NULL DEFAULT 0,
    error_count           INTEGER NOT NULL DEFAULT 0,
    input_tokens          INTEGER NOT NULL DEFAULT 0,
    output_tokens         INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens    INTEGER NOT NULL DEFAULT 0,
    accumulated_cost_yuan REAL    NOT NULL DEFAULT 0,
    first_seen_at         TEXT    NOT NULL,
    last_active_at        TEXT    NOT NULL,
    created_at            TEXT    NOT NULL,
    updated_at            TEXT    NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_proxy_sessions_external
    ON proxy_sessions(external_session_id, api_format)
    WHERE external_session_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS ix_proxy_sessions_last_active
    ON proxy_sessions(last_active_at DESC);

-- ---------------------------------------------------------------------------
-- 逐请求记录（recordProxySessionRequests 控制正文）
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS proxy_session_requests (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id          INTEGER REFERENCES proxy_sessions(id) ON DELETE SET NULL,
    client_request_id   TEXT,                                 -- X-Client-Request-Id
    method              TEXT    NOT NULL DEFAULT 'POST',
    request_path        TEXT    NOT NULL,
    api_format          TEXT    NOT NULL DEFAULT 'unknown',
    upstream_url        TEXT,
    model               TEXT,
    status_code         INTEGER,
    is_sse              INTEGER NOT NULL DEFAULT 0,
    input_tokens        INTEGER NOT NULL DEFAULT 0,
    output_tokens       INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens  INTEGER NOT NULL DEFAULT 0,
    request_bytes       INTEGER,
    response_bytes      INTEGER,
    duration_ms         INTEGER,
    first_byte_ms       INTEGER,
    error_kind          TEXT,                                 -- client_disconnected|upstream_error|timeout|...
    request_headers     TEXT,                                 -- 头白名单 JSON，绝不含 token
    request_body        TEXT,                                 -- 默认 NULL；开启时 ≤64KB
    response_body       TEXT,                                 -- 默认 NULL；开启时 ≤64KB
    body_truncated      INTEGER NOT NULL DEFAULT 0,
    started_at          TEXT    NOT NULL,
    ended_at            TEXT
);

CREATE INDEX IF NOT EXISTS ix_psr_started_at
    ON proxy_session_requests(started_at DESC);

CREATE INDEX IF NOT EXISTS ix_psr_session
    ON proxy_session_requests(session_id, started_at DESC);

CREATE INDEX IF NOT EXISTS ix_psr_status
    ON proxy_session_requests(status_code, started_at DESC);

-- ---------------------------------------------------------------------------
-- 逐请求费用：来源 upstream（aigw.cost.total_credit/100）或 estimated（model_prices）
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS proxy_request_costs (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id          INTEGER REFERENCES proxy_session_requests(id) ON DELETE CASCADE,
    session_id          INTEGER REFERENCES proxy_sessions(id) ON DELETE SET NULL,
    api_format          TEXT    NOT NULL DEFAULT 'unknown',
    model               TEXT,
    input_tokens        INTEGER NOT NULL DEFAULT 0,
    output_tokens       INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens  INTEGER NOT NULL DEFAULT 0,
    cost_yuan           REAL    NOT NULL DEFAULT 0,
    cost_source         TEXT    NOT NULL DEFAULT 'none',      -- upstream|estimated|none
    currency            TEXT    NOT NULL DEFAULT 'CNY',
    total_credit        REAL,
    trace_id            TEXT,                                 -- aigw.trace_id
    detail_json         TEXT,                                 -- 原始费用明细 JSON
    created_at          TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS ix_prc_request
    ON proxy_request_costs(request_id);

CREATE INDEX IF NOT EXISTS ix_prc_created_at
    ON proxy_request_costs(created_at DESC);

CREATE INDEX IF NOT EXISTS ix_prc_model
    ON proxy_request_costs(model, created_at DESC);

-- ---------------------------------------------------------------------------
-- 每日聚合（按 日/协议/模型/客户端 四维），代理与手动重算共用
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS usage_daily (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    day                 TEXT    NOT NULL,                     -- YYYY-MM-DD（本地时区）
    api_format          TEXT    NOT NULL DEFAULT '',
    model               TEXT    NOT NULL DEFAULT '',
    client              TEXT    NOT NULL DEFAULT '',
    request_count       INTEGER NOT NULL DEFAULT 0,
    error_count         INTEGER NOT NULL DEFAULT 0,
    input_tokens        INTEGER NOT NULL DEFAULT 0,
    output_tokens       INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens  INTEGER NOT NULL DEFAULT 0,
    cost_yuan           REAL    NOT NULL DEFAULT 0,
    total_duration_ms   INTEGER NOT NULL DEFAULT 0,
    updated_at          TEXT    NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_usage_daily_dim
    ON usage_daily(day, api_format, model, client);

CREATE INDEX IF NOT EXISTS ix_usage_daily_day
    ON usage_daily(day);

-- ---------------------------------------------------------------------------
-- 额度快照：保留上次成功值 + 原始 JSON；失败不覆盖金额（只追加状态行）
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS quota_snapshots (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    profile_id      INTEGER REFERENCES auth_profiles(id) ON DELETE CASCADE,
    quota_type      TEXT    NOT NULL DEFAULT 'global',
    status          TEXT    NOT NULL DEFAULT 'ok',            -- ok|forbidden|not_found|error|unavailable
    http_status     INTEGER,
    monthly_quota   REAL,
    used            REAL,
    remaining       REAL,
    percentage      REAL,
    reset_day       INTEGER,
    period_start    TEXT,
    reset_at        TEXT,
    timezone        TEXT,
    is_active       INTEGER,
    raw_json        TEXT,
    fetched_at      TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS ix_quota_snapshots_latest
    ON quota_snapshots(profile_id, quota_type, fetched_at DESC);

-- ---------------------------------------------------------------------------
-- 模型价格（无上游费用时兜底估算），单位：元 / 百万 token
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS model_prices (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    model                   TEXT    NOT NULL,
    input_per_mtok          REAL    NOT NULL DEFAULT 0,
    output_per_mtok         REAL    NOT NULL DEFAULT 0,
    cache_read_per_mtok     REAL    NOT NULL DEFAULT 0,
    cache_write_per_mtok    REAL    NOT NULL DEFAULT 0,
    currency                TEXT    NOT NULL DEFAULT 'CNY',
    source                  TEXT    NOT NULL DEFAULT 'manual', -- manual|seed|remote
    effective_from          TEXT,
    updated_at              TEXT    NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_model_prices_model
    ON model_prices(model);
