// @vitest-environment jsdom
import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { Login } from './Login'
import { invokeCmd } from '../lib/ipc'
import { AUTH_PENDING, LOGIN_SESSION } from '../test/fixtures'

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
