// @vitest-environment jsdom
import { render, screen, waitFor } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import App from './App'
import { invokeCmd } from './lib/ipc'
import { AUTH_UNAUTHENTICATED, PROXY, QUOTA } from './test/fixtures'
import type { CostTrend, UsageSummary } from './lib/types'

const eventHandlers = vi.hoisted(() => ({
  map: {} as Record<string, ((payload?: unknown) => void) | undefined>
}))

vi.mock('./lib/events', () => ({
  useTauriEvent: (event: string, handler: (payload?: unknown) => void) => {
    eventHandlers.map[event] = handler
  }
}))

vi.mock('./lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./lib/ipc')>()
  return { ...actual, isTauri: () => true, invokeCmd: vi.fn() }
})

const invokeMock = vi.mocked(invokeCmd)

const AUTH_OK = {
  ...AUTH_UNAUTHENTICATED,
  state: 'authenticated' as const,
  user: 'alice',
  credentials_valid: true
}

const TOTALS = {
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
}

const SUMMARY: UsageSummary = {
  today: TOTALS,
  month: TOTALS,
  daily: [],
  models: [],
  recent: [],
  generated_at: '2026-11-11T00:00:00Z'
}

const TREND: CostTrend = {
  range: '30d',
  source: 'local',
  unit: 'cny',
  points: [],
  total_cost: 0,
  peak_cost: 0,
  updated_at: null,
  generated_at: '2026-11-11T00:00:00Z',
  credit_to_yuan: 0.01
}

describe('App auth refresh', () => {
  it('keeps_authenticated_shell_when_later_auth_status_fails', async () => {
    let authCalls = 0
    invokeMock.mockImplementation(async (command) => {
      if (command === 'auth_status') {
        authCalls += 1
        if (authCalls === 1) return AUTH_OK
        throw new Error('backend timeout')
      }
      if (command === 'usage_summary') return SUMMARY
      if (command === 'quota_get') return QUOTA
      if (command === 'cost_trend_get') return TREND
      if (command === 'proxy_status') return PROXY
      if (command === 'app_info') return {}
      if (command === 'settings_get') return {}
      throw new Error(`未 mock 的命令：${String(command)}`)
    })

    render(<App />)

    await waitFor(() => expect(screen.getByText('alice')).toBeInTheDocument())
    expect(screen.queryByText('无法连接本地后端')).toBeNull()

    eventHandlers.map['app://second-instance']?.({})

    await waitFor(() => expect(screen.getByText(/登录状态异常：backend timeout/)).toBeInTheDocument())
    expect(screen.getByText('alice')).toBeInTheDocument()
    expect(screen.queryByText('无法连接本地后端')).toBeNull()
  })
})
