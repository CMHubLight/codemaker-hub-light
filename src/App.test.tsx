// @vitest-environment jsdom
import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import App from './App'
import { invokeCmd } from './lib/ipc'
import {
  AUTH_UNAUTHENTICATED,
  APP_INFO,
  EMPTY_USAGE_SUMMARY,
  PROXY,
  QUOTA,
  SETTINGS
} from './test/fixtures'
import type { CostTrend } from './lib/types'

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

/**
 * 安装默认 IPC mock；`override` 返回 undefined 表示继续走默认分支。
 * 避免每个用例重复列一遍概览/设置页要用的命令。
 */
function installInvoke(
  override?: (command: string) => Promise<unknown> | undefined
) {
  invokeMock.mockImplementation(async (command) => {
    const custom = await override?.(command)
    if (custom !== undefined) return custom
    switch (command) {
      case 'auth_status':
        return AUTH_OK
      case 'usage_summary':
        return EMPTY_USAGE_SUMMARY
      case 'quota_get':
        return QUOTA
      case 'cost_trend_get':
        return TREND
      case 'proxy_status':
        return PROXY
      case 'app_info':
        return APP_INFO
      case 'settings_get':
        return SETTINGS
      case 'usage_requests':
        return { total: 0, items: [] }
      default:
        throw new Error(`未 mock 的命令：${String(command)}`)
    }
  })
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
      if (command === 'usage_summary') return EMPTY_USAGE_SUMMARY
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
    // `waitFor` 只保证 DOM 已提交；`authRef` 由 useEffect 同步，先 flush 一轮被动副作用。
    // 否则紧接着触发的事件回调可能读到尚未更新的 ref，误判成"启动失败"（CI 上偶发）。
    await act(async () => {})

    eventHandlers.map['app://second-instance']?.({})

    await waitFor(() => expect(screen.getByText(/登录状态异常：backend timeout/)).toBeInTheDocument())
    expect(screen.getByText('alice')).toBeInTheDocument()
    expect(screen.queryByText('无法连接本地后端')).toBeNull()
  })

  it('auth_error_banner_recovers_after_retry', async () => {
    let authCalls = 0
    installInvoke(async (command) => {
      if (command !== 'auth_status') return undefined
      authCalls += 1
      if (authCalls === 2) throw new Error('backend timeout')
      return AUTH_OK
    })

    render(<App />)
    await waitFor(() => expect(screen.getByText('alice')).toBeInTheDocument())
    await act(async () => {})

    eventHandlers.map['app://second-instance']?.({})
    await waitFor(() =>
      expect(screen.getByText(/登录状态异常：backend timeout/)).toBeInTheDocument()
    )

    // N-P3-31：横幅必须自带重试入口，而不是等下一个事件自愈。
    fireEvent.click(screen.getByRole('button', { name: '重试' }))
    await waitFor(() => expect(screen.queryByText(/登录状态异常/)).toBeNull())
    expect(screen.getByText('alice')).toBeInTheDocument()
    expect(authCalls).toBe(3)
  })

  it('proxy_error_banner_opens_settings_page', async () => {
    installInvoke()
    render(<App />)
    await waitFor(() => expect(screen.getByText('alice')).toBeInTheDocument())

    act(() => {
      eventHandlers.map['proxy://error']?.({ message: '端口 15721 已被占用' })
    })
    expect(await screen.findByText(/本地代理启动失败：端口 15721 已被占用/)).toBeInTheDocument()

    // N-P3-28：横幅不只留在概览，必须能一键切到设置页排查。
    fireEvent.click(screen.getByRole('button', { name: '打开设置' }))
    await waitFor(() => expect(screen.getByRole('heading', { name: '设置' })).toBeInTheDocument())
    expect(await screen.findByRole('button', { name: '保存设置' })).toBeInTheDocument()
  })

  it('dirty_settings_requires_confirmation_before_page_switch', async () => {
    installInvoke()
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(false)
    render(<App />)
    await waitFor(() => expect(screen.getByText('alice')).toBeInTheDocument())

    fireEvent.click(screen.getByRole('button', { name: '设置' }))
    const gatewayInput = await screen.findByLabelText(/网关地址/)
    fireEvent.change(gatewayInput, { target: { value: 'https://api.example.test/changed' } })

    // 脏态必须上报到 shell：关窗确认（beforeunload）也要生效。
    const unload = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(unload)
    expect(unload.defaultPrevented).toBe(true)

    // 用户取消：留在设置页，编辑不丢。
    fireEvent.click(screen.getByRole('button', { name: '请求' }))
    expect(confirmSpy).toHaveBeenCalledTimes(1)
    expect(screen.getByRole('button', { name: '保存设置' })).toBeInTheDocument()

    // 用户确认：正常切页。
    confirmSpy.mockReturnValue(true)
    fireEvent.click(screen.getByRole('button', { name: '请求' }))
    expect(await screen.findByText('请求明细')).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '保存设置' })).toBeNull()
  })
})
