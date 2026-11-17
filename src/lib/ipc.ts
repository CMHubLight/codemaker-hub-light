import { invoke } from '@tauri-apps/api/core'
import type {
  AppInfo,
  AppLogResult,
  AppSettings,
  AuthStatus,
  CostTrend,
  Cursor,
  LoginSession,
  ProxyStatus,
  RecentRequest,
  RequestFilter,
  RequestPage,
  TrendRange,
  UsageSummary
} from './types'

/**
 * 与 `src-tauri` 注册的命令一一对应。
 *
 * 参数命名规则（踩过的坑，别再踩）：
 * - 命令**顶层**参数名一律 camelCase：Rust `from_day` / `to_day` → JS `fromDay` / `toDay`。
 *   Tauri 默认把命令参数名转成 lowerCamelCase；写成 snake_case 不会报错，
 *   而 `Option<T>` 参数会静默收到 `None`（`usage_requests` 的日期区间就是这么丢的）。
 * - 嵌套 payload（`filter`、`settings`）走 serde 反序列化，字段名保持 Rust 的 snake_case。
 */
export type CommandName =
  | 'auth_start_login'
  | 'auth_cancel_login'
  | 'auth_login_with_auth_key'
  | 'auth_poll_status'
  | 'auth_status'
  | 'auth_logout'
  | 'auth_refresh_validate'
  | 'auth_retry_renewal'
  | 'auth_submit_deep_link'
  | 'quota_get'
  | 'proxy_status'
  | 'proxy_restart'
  | 'usage_summary'
  | 'cost_trend_get'
  | 'usage_requests'
  | 'settings_get'
  | 'settings_update'
  | 'app_open_logs'
  | 'app_info'

declare global {
  interface Window {
    /** Tauri 2 注入的内部对象；普通浏览器中不存在，可用于判断当前运行环境。 */
    __TAURI_INTERNALS__?: unknown
  }
}

/** 当前是否运行在 Tauri 桌面端（而非 `pnpm dev` 的浏览器预览）。 */
export function isTauri(): boolean {
  return typeof window !== 'undefined' && window.__TAURI_INTERNALS__ !== undefined
}

/** 把 unknown 类型的异常转成可展示的中文消息。 */
export function errorMessage(error: unknown): string {
  if (error instanceof Error && error.message) return error.message
  if (typeof error === 'string' && error) return error
  return '未知错误，请重试或打开日志目录查看详情。'
}

/** 命令错误分类：前端据此给出 403/404/超时等不同文案。 */
export type CommandErrorKind =
  | 'forbidden'
  | 'not_found'
  | 'unauthenticated'
  | 'network'
  | 'timeout'
  | 'unknown'

export interface CommandError {
  kind: CommandErrorKind
  /** 原始错误文本（后端 `Result::Err(String)` 或 invoke 异常）。 */
  raw: string
}

/**
 * 命令等待超时。
 *
 * 注意：Tauri 的 `invoke` 没有取消机制，超时只让**界面止损**，Rust 侧命令仍会跑完
 * （写操作可能仍在后台执行，例如重启代理、保存设置）。
 */
export class CommandTimeoutError extends Error {
  readonly command: CommandName
  readonly timeoutMs: number

  constructor(command: CommandName, timeoutMs: number) {
    super(
      `命令 ${command} 超时（${Math.round(timeoutMs / 1000)} 秒）。请重试；若这是写操作，它可能仍在后台执行。`
    )
    this.name = 'CommandTimeoutError'
    this.command = command
    this.timeoutMs = timeoutMs
  }
}

/** 与超时竞速；超时后原任务的结果/异常被安全忽略（不会产生未处理拒绝）。 */
export async function withTimeout<T>(
  task: Promise<T>,
  ms: number,
  command: CommandName
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined
  try {
    return await Promise.race([
      task,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => reject(new CommandTimeoutError(command, ms)), ms)
      })
    ])
  } finally {
    clearTimeout(timer)
  }
}

/**
 * 分类 Rust 命令返回的错误文本。后端错误是普通字符串，没有结构化 code，
 * 因此按 docs/gateway-contract.md 里出现的 403/404 语义做保守识别。
 *
 * 状态码用词边界匹配：早期写成 `/403|404/` 子串匹配，端口号 `14030`、请求 id
 * `15404` 之类都会被误判成“权限/不存在”，把用户引向错误的排查方向（N-P3-27）。
 */
export function describeError(cause: unknown): CommandError {
  if (cause instanceof CommandTimeoutError) {
    return { kind: 'timeout', raw: cause.message }
  }
  const raw = errorMessage(cause)
  if (/\b403\b|无权限|forbidden/i.test(raw)) return { kind: 'forbidden', raw }
  if (/\b404\b|不存在|not found/i.test(raw)) return { kind: 'not_found', raw }
  if (/尚未登录|未登录|not\s*authenticated/i.test(raw)) return { kind: 'unauthenticated', raw }
  if (/网络|超时|连接|timeout|timed out|dns|tls|http|request failed|failed to/i.test(raw)) {
    return { kind: 'network', raw }
  }
  return { kind: 'unknown', raw }
}

/** 默认等待上限：10 秒（本地 SQLite/设置类命令足够）。 */
export const DEFAULT_COMMAND_TIMEOUT_MS = 10_000

/** 会打网关或做文件/端口操作的命令放宽到 30 秒。 */
const COMMAND_TIMEOUT_MS: Partial<Record<CommandName, number>> = {
  auth_start_login: 30_000,
  /** AuthKey 登录 = tokens + validate + 模型目录/key + 额度，最坏情况比 30s 长。 */
  auth_login_with_auth_key: 60_000,
  auth_submit_deep_link: 30_000,
  auth_refresh_validate: 30_000,
  auth_retry_renewal: 30_000,
  quota_get: 30_000,
  proxy_restart: 30_000,
  app_open_logs: 15_000,
  auth_poll_status: 10_000
}

/** 某条命令的等待上限。 */
export function commandTimeoutMs(command: CommandName): number {
  return COMMAND_TIMEOUT_MS[command] ?? DEFAULT_COMMAND_TIMEOUT_MS
}

/**
 * 统一 IPC 入口：Tauri 桌面端走真实 invoke（带超时）；浏览器预览返回内存 mock，
 * 保证 `pnpm dev` 不依赖 Rust 后端也能浏览全部页面。
 */
export async function invokeCmd<T>(
  command: CommandName,
  args?: Record<string, unknown>
): Promise<T> {
  if (isTauri()) {
    return withTimeout(invoke<T>(command, args), commandTimeoutMs(command), command)
  }
  const result = await mockInvoke(command, args)
  return result as T
}

// ---------------------------------------------------------------------------
// 浏览器预览用 mock 数据（仅内存，刷新即重置；严禁在此放任何真实凭据）
// ---------------------------------------------------------------------------

const MOCK_DELAY_MS = 120

const MOCK_AUTH_STATUS: AuthStatus = {
  state: 'unauthenticated',
  user: null,
  display_name: null,
  expires_at: null,
  offline_grace: false,
  message: null,
  dept: null,
  server_url: null,
  issued_at: null,
  credentials_valid: false,
  pending_poll_uuid: null,
  pending_login_started_at: null,
  deep_link_received: false,
  last_validated_at: null
}

/** 浏览器预览没有后端，login_url 为空，登录页据此提示“需在桌面端登录”。 */
const MOCK_LOGIN_SESSION: LoginSession = {
  login_url: '',
  poll_uuid: 'mock-poll-uuid',
  redirect_uri: 'codemakerhub-light://auth/callback?pollUuid=mock-poll-uuid',
  poll_interval_secs: 2,
  timeout_secs: 300,
  opened_browser: false,
  hint: '浏览器预览模式不提供真实登录。'
}

const MOCK_APP_INFO: AppInfo = {
  name: 'CodeMaker Hub Light',
  version: '0.2.3',
  tauri_version: null,
  platform: 'browser-preview',
  data_dir: null,
  log_path: null,
  identifier: 'com.codemaker.hub.light',
  arch: 'browser',
  db_path: '',
  logs_dir: '',
  proxy_port_file: '',
  gateway_url: 'https://api.example.test',
  proxy_port: 15721,
  schema_version: 0
}

/** 预览模式下代理状态可在内存里被“重启”按钮改变，便于查看运行态 UI。 */
let mockProxy: ProxyStatus = {
  running: false,
  host: '127.0.0.1',
  port: 15721,
  base_url: 'http://127.0.0.1:15721',
  gateway_url: 'https://api.example.test',
  last_error: null,
  started_at: null,
  uptime_secs: 0,
  request_count: 0,
  error_count: 0,
  port_file: '（浏览器预览无本地文件）',
  health_url: 'http://127.0.0.1:15721/health'
}

const EMPTY_TOTALS = {
  requests: 0,
  input_tokens: 0,
  output_tokens: 0,
  cache_read_tokens: 0,
  cache_write_tokens: 0,
  total_tokens: 0,
  cost_yuan: 0,
  cache_hit_rate: null,
  avg_latency_ms: null,
  error_rate: null
} as const

const DEFAULT_SETTINGS: AppSettings = {
  gateway_url: 'https://api.example.test',
  proxy_bind: '127.0.0.1',
  proxy_port: 15721,
  record_body: false,
  retention_days: 90,
  credit_to_yuan: 0.01
}

/** 预览模式下允许在页面里保存设置并在本次会话内生效。 */
let mockSettings: AppSettings = { ...DEFAULT_SETTINGS }

function clone<T>(value: T): T {
  return structuredClone(value)
}

function emptyUsageSummary(): UsageSummary {
  return {
    today: { ...EMPTY_TOTALS },
    recent: [],
    generated_at: new Date().toISOString()
  }
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms)
  })
}

// ---------------------------------------------------------------------------
// 预览 mock：请求明细
//
// 浏览器预览没有 Rust 侧 SQLite，“请求”页若永远返回空表就看不到筛选/分页行为。
// 这里生成一份固定的假数据，并严格照后端语义过滤 + keyset 分页（多取一行前瞻：
// 只有确实还有下一页时才有 next_cursor），方便在 pnpm dev 里核对翻页边界。
// ---------------------------------------------------------------------------

const MOCK_REQUEST_COUNT = 120
const MOCK_REQUEST_PROTOCOLS = ['anthropic', 'openai_chat', 'openai_responses']
const MOCK_REQUEST_MODELS = [
  'claude-sonnet-4',
  'gpt-5-codex',
  'claude-opus-4',
  'text-embedding-3'
]
const MOCK_REQUEST_STATUSES = ['success', 'success', 'success', 'error', 'cancelled']

let mockRequests: RecentRequest[] | null = null

function pad2(value: number): string {
  return `${value}`.padStart(2, '0')
}

/** 本机日期（后端 `day_local` 同口径：按本地时区取日期，不是 UTC）。 */
function localDay(date: Date): string {
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}`
}

function mockRequestRows(): RecentRequest[] {
  if (mockRequests) return mockRequests
  const base = Date.now()
  mockRequests = Array.from({ length: MOCK_REQUEST_COUNT }, (_, index) => {
    const started = new Date(base - index * 37 * 60 * 1000)
    return {
      id: `${MOCK_REQUEST_COUNT - index}`,
      created_at: started.toISOString(),
      model: MOCK_REQUEST_MODELS[index % MOCK_REQUEST_MODELS.length],
      protocol: MOCK_REQUEST_PROTOCOLS[index % MOCK_REQUEST_PROTOCOLS.length],
      client: 'unknown',
      status: MOCK_REQUEST_STATUSES[index % MOCK_REQUEST_STATUSES.length],
      latency_ms: 120 + (index % 7) * 35,
      first_byte_ms: 30 + (index % 5) * 12,
      input_tokens: 400 + index * 13,
      output_tokens: 120 + index * 7,
      cost_yuan: Number((0.0012 * ((index % 9) + 1)).toFixed(4))
    }
  })
  return mockRequests
}

function cursorOf(row: RecentRequest): Cursor {
  return {
    day_local: localDay(new Date(row.created_at)),
    started_at: row.created_at,
    id: Number(row.id)
  }
}

/** 排序键 (day_local, started_at, id) 均倒序：行严格位于游标之后（更旧）。 */
function isAfterCursor(row: RecentRequest, day: string, cursor: Cursor): boolean {
  if (day !== cursor.day_local) return day < cursor.day_local
  if (row.created_at !== cursor.started_at) return row.created_at < cursor.started_at
  return Number(row.id) < cursor.id
}

function filterMockRequests(filter: RequestFilter): RecentRequest[] {
  const keyword = filter.model?.trim().toLowerCase() ?? ''
  const from = filter.from?.slice(0, 10)
  const to = filter.to?.slice(0, 10)
  return mockRequestRows()
    .map((row) => ({ row, day: localDay(new Date(row.created_at)) }))
    .filter(({ row, day }) => {
      if (from && day < from) return false
      if (to && day > to) return false
      if (filter.protocol && row.protocol !== filter.protocol) return false
      if (filter.status && row.status !== filter.status) return false
      if (keyword && !row.model.toLowerCase().includes(keyword)) return false
      if (filter.after && !isAfterCursor(row, day, filter.after)) return false
      return true
    })
    .map(({ row }) => row)
}

async function mockInvoke(
  command: CommandName,
  args?: Record<string, unknown>
): Promise<unknown> {
  await delay(MOCK_DELAY_MS)

  switch (command) {
    case 'auth_start_login':
      return clone(MOCK_LOGIN_SESSION)
    case 'auth_login_with_auth_key':
      throw new Error('浏览器预览模式不支持 AuthKey 登录，请使用桌面版。')
    case 'auth_poll_status':
    case 'auth_status':
    case 'auth_logout':
    case 'auth_refresh_validate':
    case 'auth_retry_renewal':
    case 'auth_submit_deep_link':
    case 'auth_cancel_login':
      return clone(MOCK_AUTH_STATUS)
    /** 浏览器预览没有真实快照，额度为空；页面按“暂无额度数据”处理。 */
    case 'quota_get':
      return null
    case 'proxy_status':
      return clone(mockProxy)
    case 'proxy_restart': {
      // 预览的“重启”必须读取已保存设置里的端口/网关，否则改端口再重启还是旧地址，
      // 会让人误以为后端没生效（与 Rust 侧 restart 的真实行为对齐）。
      const port = mockSettings.proxy_port
      const host = mockSettings.proxy_bind === 'localhost' ? '127.0.0.1' : mockSettings.proxy_bind
      const baseUrl = `http://${host}:${port}`
      mockProxy = {
        ...mockProxy,
        running: true,
        host,
        port,
        base_url: baseUrl,
        gateway_url: mockSettings.gateway_url,
        health_url: `${baseUrl}/health`,
        started_at: new Date().toISOString(),
        uptime_secs: 0,
        last_error: null
      }
      return clone(mockProxy)
    }
    case 'usage_summary':
      return emptyUsageSummary()
    case 'cost_trend_get': {
      const range = (args?.range as TrendRange | undefined) ?? '24h'
      const trend: CostTrend = {
        range,
        source: range === '24h' ? 'cloud' : 'local',
        unit: range === '24h' ? 'points' : 'cny',
        points: [],
        total_cost: 0,
        peak_cost: 0,
        updated_at: null,
        generated_at: new Date().toISOString(),
        credit_to_yuan: 0.01
      }
      return trend
    }
    case 'usage_requests': {
      const filter = ((args as { filter?: RequestFilter } | undefined)?.filter ?? {}) as RequestFilter
      const matched = filterMockRequests(filter)
      const limit = Math.min(Math.max(filter.limit ?? 50, 1), 200)
      const items = matched.slice(0, limit)
      const hasMore = matched.length > items.length
      const last = items[items.length - 1]
      const page: RequestPage = {
        // 与后端一致：非首页不统计 COUNT（返回 -1），由前端沿用第一页的总数。
        total: filter.include_total === false ? -1 : matched.length,
        items: clone(items),
        next_cursor: hasMore && last ? cursorOf(last) : null,
        prev_cursor: filter.after ?? null
      }
      return page
    }
    case 'settings_get':
      return clone(mockSettings)
    case 'settings_update': {
      const patch = (args?.settings ?? {}) as Partial<AppSettings>
      mockSettings = { ...mockSettings, ...patch }
      return clone(mockSettings)
    }
    case 'app_open_logs': {
      const result: AppLogResult = {
        opened: false,
        message: '浏览器预览模式没有本地日志目录。',
        path: ''
      }
      return result
    }
    case 'app_info':
      return clone(MOCK_APP_INFO)
  }
}
