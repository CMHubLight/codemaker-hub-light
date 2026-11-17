// @vitest-environment jsdom
import { act, renderHook } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { listen } from '@tauri-apps/api/event'
import { LISTEN_RETRY_DELAYS_MS, useTauriEvent } from './events'

vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn() }))

vi.mock('./ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./ipc')>()
  return { ...actual, isTauri: () => true }
})

const listenMock = vi.mocked(listen)

afterEach(() => {
  vi.useRealTimers()
})

/** 等微任务落地：listen 的 then/catch 链不是同步的。 */
async function flushMicrotasks() {
  await act(async () => {
    await Promise.resolve()
    await Promise.resolve()
  })
}

describe('useTauriEvent', () => {
  it('retries_after_transient_listen_failure_and_receives_payload', async () => {
    vi.useFakeTimers()
    const handler = vi.fn()
    /** 捕获第二次（重试后）订阅注册的回调，手动模拟后端事件。 */
    let emit: ((payload: unknown) => void) | undefined

    listenMock
      .mockRejectedValueOnce(new Error('event system not ready'))
      .mockImplementationOnce(((_event: string, callback: (event: { payload: unknown }) => void) => {
        emit = (payload) => callback({ payload })
        return Promise.resolve(() => {})
      }) as unknown as typeof listen)

    renderHook(() => useTauriEvent('proxy://error', handler))
    await flushMicrotasks()
    // 首次订阅失败：还不能调用 handler，也不应该立刻重试（要走退避）。
    expect(listenMock).toHaveBeenCalledTimes(1)
    expect(handler).not.toHaveBeenCalled()

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LISTEN_RETRY_DELAYS_MS[0])
    })
    await flushMicrotasks()
    expect(listenMock).toHaveBeenCalledTimes(2)
    expect(listenMock.mock.calls[1]?.[0]).toBe('proxy://error')

    act(() => {
      emit?.({ message: '代理启动失败' })
    })
    expect(handler).toHaveBeenCalledWith({ message: '代理启动失败' })
  })

  it('stops_after_max_backoff_retries_when_listen_keeps_failing', async () => {
    vi.useFakeTimers()
    const handler = vi.fn()
    listenMock.mockRejectedValue(new Error('event permission denied'))

    renderHook(() => useTauriEvent('quota://updated', handler))
    await flushMicrotasks()
    expect(listenMock).toHaveBeenCalledTimes(1)

    for (const delay of LISTEN_RETRY_DELAYS_MS) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(delay)
      })
      await flushMicrotasks()
    }
    // 首次 + 3 次退避重试后彻底放弃（由 App 的 60s 兜底轮询接手）。
    expect(listenMock).toHaveBeenCalledTimes(1 + LISTEN_RETRY_DELAYS_MS.length)

    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000)
    })
    expect(listenMock).toHaveBeenCalledTimes(1 + LISTEN_RETRY_DELAYS_MS.length)
    expect(handler).not.toHaveBeenCalled()
  })

  it('disposes_late_resolved_listener_after_unmount', async () => {
    const handler = vi.fn()
    const dispose = vi.fn()
    let resolveListen: ((unlisten: () => void) => void) | undefined
    listenMock.mockImplementationOnce((() => {
      return new Promise<() => void>((resolve) => {
        resolveListen = resolve
      })
    }) as unknown as typeof listen)

    const { unmount } = renderHook(() => useTauriEvent('auth://state', handler))
    await flushMicrotasks()
    expect(resolveListen).toBeTypeOf('function')

    // listen 还没 resolve 组件就卸载了：迟到的 unlisten 必须被调用，避免泄漏订阅。
    unmount()
    await act(async () => {
      resolveListen?.(dispose)
      await Promise.resolve()
    })
    expect(dispose).toHaveBeenCalledTimes(1)
    expect(handler).not.toHaveBeenCalled()
  })
})
