import type {
  AppInfo,
  AppSettings,
  AuthStatus,
  LoginSession,
  ProxyStatus,
  QuotaInfo
} from '../lib/types'

/**
 * 测试用固定数据：与后端契约字段一一对应（`src/lib/types.ts`）。
 * 只在本目录的测试里使用；不包含任何真实凭据/会话。
 */

export const AUTH_UNAUTHENTICATED: AuthStatus = {
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

export const AUTH_PENDING: AuthStatus = {
  ...AUTH_UNAUTHENTICATED,
  state: 'pending',
  pending_poll_uuid: 'poll-uuid',
  pending_login_started_at: '2026-10-26T08:00:00Z'
}

export const LOGIN_SESSION: LoginSession = {
  login_url: 'https://login.example.test/auth?uuid=poll-uuid',
  poll_uuid: 'poll-uuid',
  redirect_uri: 'codemakerhub-light://auth/callback?pollUuid=poll-uuid',
  poll_interval_secs: 2,
  timeout_secs: 300,
  opened_browser: true,
  hint: '已打开系统浏览器；完成登录后会自动更新'
}

export const SETTINGS: AppSettings = {
  gateway_url: 'https://api.example.test',
  proxy_bind: '127.0.0.1',
  proxy_port: 15721,
  record_body: false,
  retention_days: 90,
  credit_to_yuan: 0.01
}

export const PROXY: ProxyStatus = {
  running: true,
  host: '127.0.0.1',
  port: 15721,
  base_url: 'http://127.0.0.1:15721',
  gateway_url: 'https://api.example.test',
  last_error: null,
  started_at: '2026-10-26T08:00:00Z',
  uptime_secs: 3600,
  request_count: 12,
  error_count: 0,
  port_file: 'C:\\data\\proxy.port',
  health_url: 'http://127.0.0.1:15721/health'
}

export const APP_INFO: AppInfo = {
  name: 'CodeMaker Hub Light',
  version: '0.2.0',
  tauri_version: '2.9.0',
  platform: 'windows',
  data_dir: 'C:\\data',
  log_path: 'C:\\data\\logs\\hub-light.log',
  identifier: 'com.codemaker.hub.light',
  arch: 'x86_64',
  db_path: 'C:\\data\\usage.db',
  logs_dir: 'C:\\data\\logs',
  proxy_port_file: 'C:\\data\\proxy.port',
  gateway_url: 'https://api.example.test',
  proxy_port: 15721,
  schema_version: 4
}

export const QUOTA: QuotaInfo = {
  monthly_quota: 100000,
  used: 12345.6,
  remaining: 87654.4,
  percentage: 12.3,
  reset_day: 1,
  reset_at: '2026-11-01T00:00:00Z',
  period_start: '2026-10-01T00:00:00Z',
  is_active: true,
  usage_cost: 123.45,
  currency: 'credits',
  fetched_at: '2026-10-26T08:00:00Z',
  status: 'ok',
  timezone: 'Asia/Shanghai',
  total_tokens: 12345678,
  app_tokens: {},
  app_costs: {},
  model_tokens: {},
  model_costs: {}
}

export function quotaFixture(overrides: Partial<QuotaInfo> = {}): QuotaInfo {
  return { ...QUOTA, ...overrides }
}
