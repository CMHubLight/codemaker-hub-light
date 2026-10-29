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

    fireEvent.click(screen.getByRole('button', { name: '显示' }))
    expect(screen.getByLabelText('AuthKey')).toHaveAttribute('type', 'text')
    fireEvent.click(screen.getByRole('button', { name: '隐藏' }))
    expect(screen.getByLabelText('AuthKey')).toHaveAttribute('type', 'password')
  })
})
