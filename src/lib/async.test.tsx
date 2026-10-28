// @vitest-environment jsdom
import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { useAsyncResource, useCoalescedRefresh } from './async'

/** 等一轮微任务，让 effect 里的 await 落地（不依赖真实时间）。 */
async function flushMicrotasks() {
  await act(async () => {
    await Promise.resolve()
    await Promise.resolve()
  })
}

afterEach(() => {
  vi.useRealTimers()
})

describe('useCoalescedRefresh', () => {
  it('event_burst_triggers_single_refresh：同一窗口内 3 个事件只多取一次', async () => {
    vi.useFakeTimers()
    const load = vi.fn(async () => 'data')

    function Harness() {
      const { revision, bump } = useCoalescedRefresh(50)
      const resource = useAsyncResource(`revision:${revision}`, load)
      return (
        <div>
          <span data-testid="value">{resource.data ?? 'pending'}</span>
          <span data-testid="revision">{revision}</span>
          <button
            type="button"
            onClick={() => {
              bump()
              bump()
              bump()
            }}
          >
            burst
          </button>
        </div>
      )
    }

    render(<Harness />)
    await flushMicrotasks()
    expect(load).toHaveBeenCalledTimes(1)

    // 三个信号落在同一个 50ms 合并窗口内：只应产生一次 revision 递增。
    fireEvent.click(screen.getByRole('button', { name: 'burst' }))
    await act(async () => {
      await vi.advanceTimersByTimeAsync(50)
    })
    await flushMicrotasks()

    expect(load).toHaveBeenCalledTimes(2)
    expect(screen.getByTestId('revision').textContent).toBe('1')
  })
})

describe('useAsyncResource', () => {
  it('async_resource_keeps_old_data_on_background_error：后台刷新失败保留旧数据', async () => {
    const load = vi
      .fn<() => Promise<string>>()
      .mockResolvedValueOnce('first')
      .mockRejectedValueOnce(new Error('网络请求失败'))

    function Harness() {
      const resource = useAsyncResource('resource:fixed', load)
      return (
        <div>
          <span data-testid="data">{resource.data ?? 'none'}</span>
          <span data-testid="error">{resource.error?.raw ?? 'none'}</span>
          <span data-testid="state">
            {resource.loading ? 'loading' : resource.refreshing ? 'refreshing' : 'idle'}
          </span>
          <button type="button" onClick={() => resource.reload()}>
            reload
          </button>
        </div>
      )
    }

    render(<Harness />)
    await flushMicrotasks()
    expect(screen.getByTestId('data')).toHaveTextContent('first')
    expect(screen.getByTestId('state')).toHaveTextContent('idle')

    fireEvent.click(screen.getByRole('button', { name: 'reload' }))
    await flushMicrotasks()

    expect(screen.getByTestId('error')).toHaveTextContent('网络请求失败')
    // 关键断言：失败不把已有数据清空（否则页面会闪成空态/加载态）。
    expect(screen.getByTestId('data')).toHaveTextContent('first')
    expect(screen.getByTestId('state')).toHaveTextContent('idle')
  })
})
