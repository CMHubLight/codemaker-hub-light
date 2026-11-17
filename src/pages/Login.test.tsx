// @vitest-environment jsdom
import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { Login } from './Login'
import { invokeCmd } from '../lib/ipc'
import { AUTH_PENDING, AUTH_UNAUTHENTICATED, LOGIN_SESSION } from '../test/fixtures'

// 事件订阅在 jsdom 里没有 Tauri 后端：把 handler 捕获下来，手动模拟深链到达。
const eventHandlers = vi.hoisted(() => ({
  map: {} as Record<string, ((payload?: unknown) => void) | undefined>
}))

vi.mock('../lib/events', () => ({
  useTauriEvent: (event: string, handler: (payload?: unknown) => void) => {
    eventHandlers.map[event] = handler
  }
}))

vi.mock('../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/ipc')>()
  return { ...actual, isTauri: () => true, invokeCmd: vi.fn() }
})

const invokeMock = vi.mocked(invokeCmd)

/** 等一轮微任务，让 startLogin / poll 的 await 落地。 */
async function flushMicrotasks() {
  await act(async () => {
    await Promise.resolve()
    await Promise.resolve()
  })
}

afterEach(() => {
  vi.useRealTimers()
})

describe('Login 轮询', () => {
  it('login_poll_is_single_flight：深链立即轮询不会和定时轮询并发成两条链', async () => {
    vi.useFakeTimers()

    let active = 0
    let maxActive = 0
    let pollCalls = 0
    let releaseFirstPoll: (() => void) | undefined

    invokeMock.mockImplementation(async (command) => {
      if (command === 'auth_start_login') return LOGIN_SESSION
      if (command === 'auth_poll_status') {
        pollCalls += 1
        active += 1
        maxActive = Math.max(maxActive, active)
        if (pollCalls === 1) {
          // 第一次轮询挂起，模拟慢网关：此时深链事件到达。
          await new Promise<void>((resolve) => {
            releaseFirstPoll = resolve
          })
        }
        active -= 1
        return AUTH_PENDING
      }
      throw new Error(`未 mock 的命令：${String(command)}`)
    })

    render(<Login previewMode={false} onAuthenticated={vi.fn()} onPreviewSignIn={vi.fn()} />)
    fireEvent.click(screen.getByRole('button', { name: '登录' }))
    await flushMicrotasks()
    expect(pollCalls).toBe(0)

    // 2s 间隔的第一次轮询：命令挂起。
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2000)
    })
    expect(pollCalls).toBe(1)

    // 深链在轮询在飞时到达：只排队，不并发发起第二条轮询。
    act(() => {
      eventHandlers.map['auth://callback']?.()
    })
    expect(pollCalls).toBe(1)
    expect(maxActive).toBe(1)

    // 释放第一次轮询：排队的立即轮询补一次，仍然不并发。
    await act(async () => {
      releaseFirstPoll?.()
      releaseFirstPoll = undefined
    })
    await flushMicrotasks()
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0)
    })
    await flushMicrotasks()

    expect(pollCalls).toBe(2)
    expect(maxActive).toBe(1)
  })
})

describe('Login AuthKey 直登', () => {
  const AUTH_OK = {
    ...AUTH_UNAUTHENTICATED,
    state: 'authenticated' as const,
    user: 'alice',
    credentials_valid: true
  }

  function renderAuthKeyPage() {
    const onAuthenticated = vi.fn()
    render(
      <Login
        previewMode={false}
        onAuthenticated={onAuthenticated}
        onPreviewSignIn={vi.fn()}
      />
    )
    return { onAuthenticated }
  }

  it('empty_fields_show_local_error_without_invoking_backend', async () => {
    const { onAuthenticated } = renderAuthKeyPage()
    fireEvent.click(screen.getByRole('button', { name: 'AuthKey 登录' }))
    await flushMicrotasks()

    expect(screen.getByRole('alert').textContent).toContain('请填写账号名和 AuthKey')
    expect(invokeMock).not.toHaveBeenCalledWith('auth_login_with_auth_key', expect.anything())
    expect(onAuthenticated).not.toHaveBeenCalled()
  })

  it('submits_trimmed_user_and_key_and_finishes_login', async () => {
    invokeMock.mockImplementation(async (command) => {
      if (command === 'auth_login_with_auth_key') return AUTH_OK
      throw new Error(`未 mock 的命令：${String(command)}`)
    })
    const { onAuthenticated } = renderAuthKeyPage()

    fireEvent.change(screen.getByLabelText('账号名'), { target: { value: ' alice ' } })
    fireEvent.change(screen.getByLabelText('AuthKey'), { target: { value: ' secret-key ' } })
    fireEvent.click(screen.getByRole('button', { name: 'AuthKey 登录' }))
    await flushMicrotasks()

    expect(invokeMock).toHaveBeenCalledWith('auth_login_with_auth_key', {
      user: 'alice',
      authKey: 'secret-key'
    })
    expect(onAuthenticated).toHaveBeenCalledTimes(1)
  })

  it('backend_rejection_is_shown_and_login_is_not_finished', async () => {
    invokeMock.mockImplementation(async (command) => {
      if (command === 'auth_login_with_auth_key') {
        throw new Error('凭据被服务端拒绝：账号名或 AuthKey 不正确。')
      }
      throw new Error(`未 mock 的命令：${String(command)}`)
    })
    const { onAuthenticated } = renderAuthKeyPage()

    fireEvent.change(screen.getByLabelText('账号名'), { target: { value: 'alice' } })
    fireEvent.change(screen.getByLabelText('AuthKey'), { target: { value: 'wrong' } })
    fireEvent.click(screen.getByRole('button', { name: 'AuthKey 登录' }))
    await flushMicrotasks()

    expect(screen.getByRole('alert').textContent).toContain('账号名或 AuthKey 不正确')
    expect(onAuthenticated).not.toHaveBeenCalled()
  })

  it('key_visibility_can_be_toggled', () => {
    renderAuthKeyPage()
    const keyInput = screen.getByLabelText('AuthKey')
    expect(keyInput).toHaveAttribute('type', 'password')

    const showButton = screen.getByRole('button', { name: '显示' })
    expect(showButton).toHaveAttribute('aria-pressed', 'false')
    fireEvent.click(showButton)
    expect(screen.getByLabelText('AuthKey')).toHaveAttribute('type', 'text')
    expect(screen.getByRole('button', { name: '隐藏' })).toHaveAttribute('aria-pressed', 'true')
    fireEvent.click(screen.getByRole('button', { name: '隐藏' }))
    expect(screen.getByLabelText('AuthKey')).toHaveAttribute('type', 'password')
  })

  it('pressing_enter_in_authkey_input_submits', async () => {
    invokeMock.mockImplementation(async (command) => {
      if (command === 'auth_login_with_auth_key') return AUTH_OK
      throw new Error(`未 mock 的命令：${String(command)}`)
    })
    const { onAuthenticated } = renderAuthKeyPage()

    fireEvent.change(screen.getByLabelText('账号名'), { target: { value: 'alice' } })
    fireEvent.change(screen.getByLabelText('AuthKey'), { target: { value: 'secret-key' } })
    fireEvent.keyDown(screen.getByLabelText('AuthKey'), { key: 'Enter' })
    await flushMicrotasks()

    expect(invokeMock).toHaveBeenCalledWith('auth_login_with_auth_key', {
      user: 'alice',
      authKey: 'secret-key'
    })
    expect(onAuthenticated).toHaveBeenCalledTimes(1)
  })

  it('authkey_login_disables_browser_login_button_while_in_flight', async () => {
    let releaseAuthKey: (() => void) | undefined
    invokeMock.mockImplementation(async (command) => {
      if (command === 'auth_login_with_auth_key') {
        await new Promise<void>((resolve) => {
          releaseAuthKey = resolve
        })
        return AUTH_OK
      }
      throw new Error(`未 mock 的命令：${String(command)}`)
    })
    const { onAuthenticated } = renderAuthKeyPage()

    fireEvent.change(screen.getByLabelText('账号名'), { target: { value: 'alice' } })
    fireEvent.change(screen.getByLabelText('AuthKey'), { target: { value: 'secret-key' } })
    fireEvent.click(screen.getByRole('button', { name: 'AuthKey 登录' }))
    await flushMicrotasks()

    // N-P2-4：AuthKey 在飞时主「登录」按钮必须禁用，避免两条登录流程竞争收尾。
    const browserButton = screen.getByRole('button', { name: 'AuthKey 登录中…' })
    expect(browserButton).toBeDisabled()
    expect(screen.getByRole('button', { name: '登录中…' })).toBeDisabled()

    await act(async () => {
      releaseAuthKey?.()
    })
    await flushMicrotasks()
    expect(onAuthenticated).toHaveBeenCalledTimes(1)
  })

  it('authkey_login_is_disabled_while_waiting_for_browser', async () => {
    vi.useFakeTimers()
    invokeMock.mockImplementation(async (command) => {
      if (command === 'auth_start_login') return LOGIN_SESSION
      if (command === 'auth_poll_status') return AUTH_PENDING
      throw new Error(`未 mock 的命令：${String(command)}`)
    })
    const { onAuthenticated } = renderAuthKeyPage()

    fireEvent.click(screen.getByRole('button', { name: '登录' }))
    await flushMicrotasks()

    expect(screen.getByText(/等待浏览器登录中/)).toBeInTheDocument()
    const authKeyButton = screen.getByRole('button', { name: 'AuthKey 登录' })
    expect(authKeyButton).toBeDisabled()

    fireEvent.click(authKeyButton)
    await flushMicrotasks()
    expect(invokeMock).not.toHaveBeenCalledWith('auth_login_with_auth_key', expect.anything())
    expect(onAuthenticated).not.toHaveBeenCalled()
  })
})

describe('Login 取消等待', () => {
  function mockWaitingLogin() {
    invokeMock.mockImplementation(async (command) => {
      if (command === 'auth_start_login') return LOGIN_SESSION
      if (command === 'auth_poll_status') return AUTH_PENDING
      if (command === 'auth_cancel_login') return AUTH_UNAUTHENTICATED
      throw new Error(`未 mock 的命令：${String(command)}`)
    })
  }

  function commandCalls(command: string): number {
    return invokeMock.mock.calls.filter(([name]) => name === command).length
  }

  it('cancel_waiting_calls_backend_and_returns_to_idle', async () => {
    mockWaitingLogin()
    render(<Login previewMode={false} onAuthenticated={vi.fn()} onPreviewSignIn={vi.fn()} />)

    fireEvent.click(screen.getByRole('button', { name: '登录' }))
    await flushMicrotasks()
    expect(screen.getByRole('button', { name: '取消等待' })).toBeInTheDocument()

    fireEvent.click(screen.getByRole('button', { name: '取消等待' }))
    await flushMicrotasks()

    // N-P2-8：取消必须通知后端清 pending，不能只改本地 state。
    expect(commandCalls('auth_cancel_login')).toBe(1)
    expect(screen.getByRole('button', { name: '登录' })).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '取消等待' })).toBeNull()
  })

  it('late_pending_event_after_cancel_does_not_restore_waiting', async () => {
    mockWaitingLogin()
    render(<Login previewMode={false} onAuthenticated={vi.fn()} onPreviewSignIn={vi.fn()} />)

    fireEvent.click(screen.getByRole('button', { name: '登录' }))
    await flushMicrotasks()
    fireEvent.click(screen.getByRole('button', { name: '取消等待' }))
    await flushMicrotasks()

    // 取消后迟到的 pending 广播不能把页面又拽回等待态（N-P3-30）。
    act(() => {
      eventHandlers.map['auth://state']?.({ ...AUTH_PENDING, state: 'pending' })
    })
    await flushMicrotasks()
    expect(screen.getByRole('heading', { name: '登录以继续' })).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '取消等待' })).toBeNull()
  })

  it('waiting_times_out_from_backend_pending_started_at_without_session', async () => {
    vi.useFakeTimers()
    invokeMock.mockImplementation(async (command) => {
      throw new Error(`未 mock 的命令：${String(command)}`)
    })
    // 应用重启后本地没有 session/startedAt：只有后端 pending 起点能算出已等待太久。
    render(
      <Login
        previewMode={false}
        initialStatus={{
          ...AUTH_PENDING,
          pending_login_started_at: new Date(Date.now() - 60 * 60 * 1000).toISOString()
        }}
        onAuthenticated={vi.fn()}
        onPreviewSignIn={vi.fn()}
      />
    )

    await act(async () => {
      await vi.advanceTimersByTimeAsync(2000)
    })
    await flushMicrotasks()
    expect(screen.getByText(/登录超时/)).toBeInTheDocument()
    expect(commandCalls('auth_poll_status')).toBe(0)
  })
})

describe('Login 注销告警', () => {
  it('logout_warning_is_visible：注销后磁盘凭据没清干净的告警必须显示在登录页', () => {
    const message = '退出登录未能清除本地凭据（凭据文件读写失败：拒绝访问）。'
    render(
      <Login
        previewMode={false}
        initialStatus={{ ...AUTH_UNAUTHENTICATED, message }}
        onAuthenticated={vi.fn()}
        onPreviewSignIn={vi.fn()}
      />
    )

    expect(screen.getByText(/未能清除本地凭据/)).toBeInTheDocument()
  })
})
