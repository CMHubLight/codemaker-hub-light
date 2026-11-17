// @vitest-environment jsdom
import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import { Requests } from './Requests'
import { invokeCmd } from '../lib/ipc'
import type { Cursor, RecentRequest, RequestFilter } from '../lib/types'

vi.mock('../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/ipc')>()
  return { ...actual, isTauri: () => true, invokeCmd: vi.fn() }
})

const invokeMock = vi.mocked(invokeCmd)

/** 两页之间用于「下一页」的 keyset 游标（字段名即后端 serde 形状）。 */
const PAGE_ONE_CURSOR: Cursor = {
  day_local: '2026-10-25',
  started_at: '2026-10-25T10:00:00Z',
  id: 2
}

function request(id: string, model: string): RecentRequest {
  return {
    id,
    created_at: `2026-10-25T10:0${id}:00Z`,
    model,
    protocol: 'anthropic',
    client: 'unknown',
    status: 'success',
    latency_ms: 120,
    first_byte_ms: 45,
    input_tokens: 10,
    output_tokens: 5,
    cost_yuan: 0.001
  }
}

/** 从 mock 调用里取嵌套 filter（嵌套 payload 字段 snake_case，与 Rust 一致）。 */
function filterOf(args: unknown): RequestFilter {
  return (args as { filter?: RequestFilter } | undefined)?.filter ?? {}
}

describe('Requests 分页（keyset 游标）', () => {
  it('event_refresh_on_later_page_refetches_current_page_without_resetting_cursor', async () => {
    const user = userEvent.setup()
    const filters: RequestFilter[] = []
    /** 第二页每次返回的内容：事件刷新后应看到新内容，证明重取的是当前页。 */
    let secondPageModel = 'model-1'

    invokeMock.mockImplementation(async (command, args) => {
      if (command !== 'usage_requests') throw new Error(`未 mock 的命令：${String(command)}`)
      const filter = filterOf(args)
      filters.push(filter)
      if (filter.after == null) {
        return {
          total: 120,
          items: [request('3', 'model-3')],
          next_cursor: PAGE_ONE_CURSOR,
          prev_cursor: null
        }
      }
      return {
        total: -1,
        items: [request('1', secondPageModel)],
        next_cursor: null,
        prev_cursor: filter.after
      }
    })

    const { rerender } = render(<Requests revision={0} />)
    await waitFor(() => expect(screen.getByText('第 1 / 3 页')).toBeInTheDocument())

    await user.click(screen.getByRole('button', { name: '下一页' }))
    await waitFor(() => expect(screen.getByText('第 2 / 3 页')).toBeInTheDocument())
    expect(screen.getByText('model-1')).toBeInTheDocument()

    // 后台额度/用量事件刷新：停在第 2 页时只重取第 2 页（同一游标），
    // 旧实现会 setPageIndex(0) 把用户打回第一页（旧 P2-4）。
    secondPageModel = 'model-1-refreshed'
    rerender(<Requests revision={1} />)

    await waitFor(() => expect(screen.getByText('model-1-refreshed')).toBeInTheDocument())
    expect(screen.getByText('第 2 / 3 页')).toBeInTheDocument()
    expect(screen.queryByText('model-3')).toBeNull()
    // 第 2 页的第三次请求仍是同一游标，且不统计 total。
    expect(filters[2]?.after).toEqual(PAGE_ONE_CURSOR)
    expect(filters[2]?.include_total).toBe(false)
  })

  it('first_page_event_refresh_clears_stale_total', async () => {
    let firstPageTotal = 120

    invokeMock.mockImplementation(async (command, args) => {
      if (command !== 'usage_requests') throw new Error(`未 mock 的命令：${String(command)}`)
      const filter = filterOf(args)
      if (filter.after == null) {
        return {
          total: firstPageTotal,
          items: [request('3', 'model-3')],
          next_cursor: PAGE_ONE_CURSOR,
          prev_cursor: null
        }
      }
      return { total: -1, items: [], next_cursor: null, prev_cursor: filter.after }
    })

    const { rerender } = render(<Requests revision={0} />)
    await waitFor(() => expect(screen.getByText('第 1 / 3 页')).toBeInTheDocument())

    // 后台清理让总数缩到 60（2 页）：第一页的事件刷新必须重算 total，
    // 不能残留旧的 120（旧 P2-6）。
    firstPageTotal = 60
    rerender(<Requests revision={1} />)

    await waitFor(() => expect(screen.getByText('第 1 / 2 页')).toBeInTheDocument())
    expect(screen.queryByText(/第 \d+ \/ 3 页/)).toBeNull()
  })

  it('filter_change_clears_previous_total', async () => {
    const user = userEvent.setup()
    let filterCalls = 0
    invokeMock.mockImplementation(async (command, args) => {
      if (command !== 'usage_requests') throw new Error(`未 mock 的命令：${String(command)}`)
      filterCalls += 1
      const filter = filterOf(args)
      if (filter.status === 'error') return { total: 10, items: [], next_cursor: null }
      return {
        total: 120,
        items: [request('3', 'model-3')],
        next_cursor: PAGE_ONE_CURSOR,
        prev_cursor: null
      }
    })

    render(<Requests revision={0} />)
    await waitFor(() => expect(screen.getByText(/共 120 条 · 每页 50 条/)).toBeInTheDocument())

    // 换筛选条件后总数必须重新统计：旧 total 属于上一条件，不能沿用。
    await user.selectOptions(screen.getByLabelText('状态'), 'error')
    await user.click(screen.getByRole('button', { name: '查询' }))

    await waitFor(() => expect(screen.getByText(/共 10 条 · 每页 50 条/)).toBeInTheDocument())
    expect(screen.getByText('第 1 / 1 页')).toBeInTheDocument()
    expect(filterCalls).toBe(2)
  })

  it('empty_later_page_shows_guidance_and_returns_to_first_page', async () => {
    const user = userEvent.setup()
    const filters: RequestFilter[] = []
    invokeMock.mockImplementation(async (command, args) => {
      if (command !== 'usage_requests') throw new Error(`未 mock 的命令：${String(command)}`)
      const filter = filterOf(args)
      filters.push(filter)
      if (filter.after == null) {
        return {
          total: 120,
          items: [request('3', 'model-3')],
          next_cursor: PAGE_ONE_CURSOR,
          prev_cursor: null
        }
      }
      // 后端多取一行后不再产生“整数倍满页空页”，但数据被清理时仍可能拿到空页。
      return { total: -1, items: [], next_cursor: null, prev_cursor: filter.after }
    })

    render(<Requests revision={0} />)
    await waitFor(() => expect(screen.getByText('第 1 / 3 页')).toBeInTheDocument())

    await user.click(screen.getByRole('button', { name: '下一页' }))
    await waitFor(() => expect(screen.getByText('本页暂无数据')).toBeInTheDocument())

    // 空页不能只留一句“加载失败”，要给明确的回退入口（旧 P2-6）。
    const backButton = screen.getByRole('button', { name: '回到第一页' })
    await user.click(backButton)

    await waitFor(() => expect(screen.getByText('第 1 / 3 页')).toBeInTheDocument())
    expect(screen.getByText('model-3')).toBeInTheDocument()
    expect(filters.at(-1)?.after).toBeUndefined()
  })

  it('retry_keeps_querying_state_until_request_settles', async () => {
    let releaseRetry: (() => void) | undefined
    let calls = 0
    invokeMock.mockImplementation(async (command) => {
      if (command !== 'usage_requests') throw new Error(`未 mock 的命令：${String(command)}`)
      calls += 1
      if (calls === 1) throw new Error('db busy')
      await new Promise<void>((resolve) => {
        releaseRetry = resolve
      })
      return { total: 50, items: [request('3', 'model-3')], next_cursor: null }
    })

    render(<Requests revision={0} />)
    await waitFor(() => expect(screen.getByText(/读取请求列表失败：db busy/)).toBeInTheDocument())

    // 点击重试后，新请求真正开始前也不能提前恢复“查询”按钮（旧 P3-6）。
    fireEvent.click(screen.getByRole('button', { name: '重试' }))
    expect(screen.getByRole('button', { name: '查询中…' })).toBeDisabled()

    releaseRetry?.()
    await waitFor(() => expect(screen.getByText('model-3')).toBeInTheDocument())
    expect(screen.getByRole('button', { name: '查询' })).toBeEnabled()
  })

  it('failed_next_page_does_not_render_previous_rows_under_new_page', async () => {
    const user = userEvent.setup()
    const filters: RequestFilter[] = []

    invokeMock.mockImplementation(async (command, args) => {
      if (command !== 'usage_requests') throw new Error(`未 mock 的命令：${String(command)}`)
      const filter = filterOf(args)
      filters.push(filter)
      if (filter.after == null) {
        return {
          total: 60,
          items: [request('3', 'model-3'), request('2', 'model-2')],
          next_cursor: PAGE_ONE_CURSOR,
          prev_cursor: null
        }
      }
      throw new Error('db busy')
    })

    render(<Requests revision={0} />)
    await waitFor(() => expect(screen.getByText('model-3')).toBeInTheDocument())

    await user.click(screen.getByRole('button', { name: '下一页' }))

    await waitFor(() => expect(screen.getByText(/读取请求列表失败：db busy/)).toBeInTheDocument())
    expect(screen.queryByText('model-3')).toBeNull()
    expect(screen.queryByText('model-2')).toBeNull()
    expect(screen.getByText('请求列表加载失败')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '下一页' })).toBeDisabled()
    expect(filters[1]?.after).toEqual(PAGE_ONE_CURSOR)
    expect(filters).toHaveLength(2)
  })

  it('cursor_pagination_does_not_duplicate_or_skip：中途插入行也不重复、不跳行', async () => {
    const user = userEvent.setup()
    const filters: RequestFilter[] = []
    let inserted = false

    invokeMock.mockImplementation(async (command, args) => {
      if (command !== 'usage_requests') throw new Error(`未 mock 的命令：${String(command)}`)
      const filter = filterOf(args)
      filters.push(filter)
      if (filter.after == null) {
        // 第一页：按 (day_local, started_at, id) 倒序的最新两条。
        const page = {
          total: 60,
          items: [request('3', 'model-3'), request('2', 'model-2')],
          next_cursor: PAGE_ONE_CURSOR,
          prev_cursor: null
        }
        // 两页请求之间插入一条更新的行：keyset 第二页不会看到它，也不会重复/跳过旧行。
        inserted = true
        return page
      }
      // 第二页：严格小于游标的旧行；中途插入的 model-4 不在本页。
      return {
        total: -1,
        items: [request('1', 'model-1')],
        next_cursor: null,
        prev_cursor: filter.after
      }
    })

    render(<Requests revision={0} />)
    await waitFor(() => expect(screen.getByText('model-3')).toBeInTheDocument())
    expect(screen.getByText('model-2')).toBeInTheDocument()
    // 总数与行数据同帧可见（不要把这段放进行断言之后同步执行：旧实现靠额外一帧同步 state）。
    await waitFor(() => expect(screen.getByText(/共 60 条 · 每页 50 条/)).toBeInTheDocument())

    // 第一页必须统计 total，且不带游标。
    expect(filters[0]?.include_total).toBe(true)
    expect(filters[0]?.after).toBeUndefined()

    await user.click(screen.getByRole('button', { name: '下一页' }))
    await waitFor(() => expect(screen.getByText('model-1')).toBeInTheDocument())
    await waitFor(() => expect(screen.getByText('第 2 / 2 页')).toBeInTheDocument())

    // 第二页请求：原样带回上一页的 next_cursor，且不再统计 total。
    const secondFilter = filters[1]
    expect(secondFilter?.after).toEqual(PAGE_ONE_CURSOR)
    expect(secondFilter?.include_total).toBe(false)
    expect(secondFilter).not.toHaveProperty('offset')

    // 页内容整体替换：不重复显示旧行，也没有被跳过的旧行；中途插入的 model-4 不在本页。
    expect(inserted).toBe(true)
    expect(screen.queryByText('model-2')).toBeNull()
    expect(screen.queryByText('model-4')).toBeNull()
    expect(screen.getByRole('button', { name: '下一页' })).toBeDisabled()
  })
})
