/**
 * 前端 ↔ Rust 命令层/事件层的数据契约。
 *
 * 字段名与 `src-tauri` 的 serde 结构保持一致（snake_case），来源：
 * `src-tauri/src/auth/mod.rs`（AuthStatus）、`src-tauri/src/usage/mod.rs`
 * （UsageSummary/QuotaInfo/RequestPage）、`src-tauri/src/proxy/server.rs`（ProxyStatus）、
 * `src-tauri/src/settings.rs`（Settings）、`src-tauri/src/commands/{auth,app}.rs`。
 *
 * 安全约定：这些结构里永远不出现 access_token / auth_key；事件只带非敏感字段。
 */

/** 登录态。`offline_grace` 在界面上仍算“已登录”（state 为 authenticated）。 */
export type AuthState =
  | 'unauthenticated'
  | 'pending'
  | 'authenticated'
  | 'reauth_required'
  | 'error'

export interface AuthStatus {
  state: AuthState
  /** SSO 账号（用户名）；未登录为 null。 */
  user: string | null
  display_name: string | null
  /** Unix 秒（access token 过期时间）。 */
  expires_at: number | null
  /** 网络不可用时的离线宽限期（state 仍为 authenticated）。 */
  offline_grace: boolean
  /** 给用户看的状态说明（错误原因等）。 */
  message: string | null
  // ---- 诊断附加字段（后端 AuthStatus 全量返回） ----
  dept: string | null
  server_url: string | null
  /** Unix 秒。 */
  issued_at: number | null
  credentials_valid: boolean
  pending_poll_uuid: string | null
  /** RFC3339 字符串。 */
  pending_login_started_at: string | null
  deep_link_received: boolean
  /** RFC3339 字符串。 */
  last_validated_at: string | null
}

/** `auth_start_login` 的返回值。 */
export interface LoginSession {
  /** 登录页地址；为空表示当前环境不支持登录（浏览器预览）。 */
  login_url: string
  /** 轮询登录结果用的一次性标识。 */
  poll_uuid: string
  redirect_uri: string
  /** 建议轮询间隔（秒）。 */
  poll_interval_secs: number
  /** 登录总超时（秒）。 */
  timeout_secs: number
  /** 后端是否成功唤起系统浏览器。 */
  opened_browser: boolean
  hint: string
}

/** 月度额度；字段来自网关 `/api/v1/user_quota/usage`，后端负责别名归一。 */
export interface QuotaInfo {
  monthly_quota: number | null
  used: number | null
  remaining: number | null
  /** 已用百分比（0~100）；字段缺失时前端按 used/monthly_quota 计算。 */
  percentage: number | null
  /** 每月重置日（1~31）。 */
  reset_day: number | null
  /** ISO 时间字符串。 */
  reset_at: string | null
  period_start: string | null
  is_active: boolean
  usage_cost: number | null
  currency: string
  /** ISO 时间字符串（快照时间）。 */
  fetched_at: string
  /** ok | stale | error 等后端标记。 */
  status: string
  timezone: string | null
  // ---- 云端月度用量分布（quota current_month） ----
  total_tokens: number | null
  app_tokens: Record<string, number>
  app_costs: Record<string, number>
  model_tokens: Record<string, number>
  model_costs: Record<string, number>
}

export type TrendRange = '24h' | '30d'

export interface CostTrendPoint {
  label: string
  at: string
  cost: number
  tokens: number | null
}

/** 费用曲线：24h 云端积分 / 30d 本机人民币。 */
export interface CostTrend {
  range: TrendRange
  source: 'cloud' | 'local'
  unit: 'points' | 'cny'
  points: CostTrendPoint[]
  total_cost: number
  peak_cost: number
  updated_at: string | null
  generated_at: string
  /** 云端积分折算人民币比例；30d 本机模式不使用。 */
  credit_to_yuan: number
}

export interface ProxyStatus {
  running: boolean
  host: string
  port: number
  /** 客户端应使用的地址，例如 http://127.0.0.1:15721 */
  base_url: string
  gateway_url: string
  last_error: string | null
  // ---- 诊断附加字段 ----
  started_at: string | null
  uptime_secs: number
  request_count: number
  error_count: number
  port_file: string
  health_url: string
}

/** token 四元组 + 费用等聚合指标。 */
export interface UsageTotals {
  requests: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_write_tokens: number
  total_tokens: number
  cost_yuan: number
  /** 0~1；无数据为 null。 */
  cache_hit_rate: number | null
  avg_latency_ms: number | null
  /** 0~1；无数据为 null。 */
  error_rate: number | null
}

export interface DailyUsage {
  /** YYYY-MM-DD */
  date: string
  requests: number
  tokens: number
  cost_yuan: number
}

export interface ModelUsage {
  model: string
  requests: number
  tokens: number
  cost_yuan: number
}

export interface RecentRequest {
  id: string
  /** ISO 时间字符串。 */
  created_at: string
  model: string
  /** anthropic | openai_chat | openai_responses */
  protocol: string
  client: string
  /** success | error | cancelled */
  status: string
  latency_ms: number | null
  input_tokens: number
  output_tokens: number
  cost_yuan: number
}

export interface UsageSummary {
  today: UsageTotals
  month: UsageTotals
  /** 最近 30 天，按日期升序。 */
  daily: DailyUsage[]
  models: ModelUsage[]
  recent: RecentRequest[]
  generated_at: string
}

/**
 * keyset 分页游标：上一页最后一行的排序键（`day_local, started_at, id`）。
 * 字段名与 Rust `db::sql::Cursor` 的 serde 形状一致（snake_case），原样传回
 * `RequestFilter.after` 即可继续向后翻页。
 */
export interface Cursor {
  day_local: string
  started_at: string
  id: number
}

export interface RequestFilter {
  /** 起始时间（ISO 日期或时间）。 */
  from?: string
  to?: string
  model?: string
  protocol?: string
  client?: string
  status?: string
  offset?: number
  limit?: number
  /** keyset 游标：把上一页的 `next_cursor` 原样传回；与 offset 同时给时游标优先。 */
  after?: Cursor
  /** 是否返回 `total`（默认 true）；翻页传 false 可省 COUNT，此时 `total` 为 -1。 */
  include_total?: boolean
}

export interface RequestPage {
  /** 满足筛选的总条数；`include_total = false` 时后端返回 -1（未统计）。 */
  total: number
  items: RecentRequest[]
  /** 满页时给出的下一页游标；没有更多数据为 null/缺省。 */
  next_cursor?: Cursor | null
  /** 本页所用游标（首页为 null）。 */
  prev_cursor?: Cursor | null
}

/** `settings_get` 返回的完整设置（后端 Settings）。 */
export interface AppSettings {
  gateway_url: string
  /** 只允许回环地址。 */
  proxy_bind: string
  proxy_port: number
  /** 是否记录请求/响应正文（默认关闭，按 64KB 截断）。 */
  record_body: boolean
  /** 用量明细保留天数（默认 90）。 */
  retention_days: number
  /** `aigw.cost.total_credit` → 人民币元换算系数。 */
  credit_to_yuan: number
}

/** `settings_update` 接受的局部补丁：未提交字段保持后端原值。 */
export type SettingsPatch = Partial<AppSettings>

export interface AppInfo {
  name: string
  version: string
  tauri_version: string | null
  /** windows | macos | linux | browser-preview */
  platform: string
  data_dir: string | null
  log_path: string | null
  identifier: string
  arch: string
  db_path: string
  logs_dir: string
  proxy_port_file: string
  gateway_url: string
  proxy_port: number
  schema_version: number
}

export interface UsageExportResult {
  saved: boolean
  path: string | null
  message: string | null
  rows: number
  bytes: number
  /** 行数超过导出上限（20 万行）被截断，界面需提示缩小时间范围。 */
  truncated?: boolean
}

export interface AppLogResult {
  opened: boolean
  message: string | null
  path: string
}

// ---------------------------------------------------------------------------
// Tauri 事件 payload（src-tauri/src/lib.rs、commands/auth.rs、commands/proxy.rs）
// ---------------------------------------------------------------------------

/** 深链到达事件：只带非敏感字段，绝不含 token。 */
export interface AuthCallbackEvent {
  poll_uuid: string | null
  has_credentials: boolean
}

export interface ProxyErrorEvent {
  message: string
}
