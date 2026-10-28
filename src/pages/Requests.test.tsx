// @vitest-environment jsdom
import { render, screen, waitFor } from '@testing-library/react'
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
  it('pager_resets_to_first_page_when_total_shrinks：事件刷新后回第一页，不出现越界页码', async () => {
    const user = userEvent.setup()
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

    await user.click(screen.getByRole('button', { name: '下一页' }))
    await waitFor(() => expect(screen.getByText('第 2 / 3 页')).toBeInTheDocument())

    // 后台清理让总数缩到 60（2 页）：事件刷新会静默回到第一页重取 total，
    // 因此不会出现“第 3 / 2 页”的空表（keyset 下 total 只在第一页统计）。
    firstPageTotal = 60
    rerender(<Requests revision={1} />)

    await waitFor(() => expect(screen.getByText('第 1 / 2 页')).toBeInTheDocument())
    expect(screen.queryByText(/第 \d+ \/ 3 页/)).toBeNull()
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

  it('export_truncated_message_is_shown：导出达到 20 万行上限时提示已截断', async () => {
    const user = userEvent.setup()

    invokeMock.mockImplementation(async (command) => {
      if (command === 'usage_requests') {
        return { total: 0, items: [], next_cursor: null, prev_cursor: null }
      }
      if (command === 'usage_export_csv') {
        return {
          saved: true,
          path: 'C:\\data\\exports\\usage.csv',
          message: null,
          rows: 200_000,
          bytes: 1024,
          truncated: true
        }
      }
      throw new Error(`未 mock 的命令：${String(command)}`)
    })

    render(<Requests revision={0} />)
    await user.click(screen.getByRole('button', { name: '导出 CSV' }))

    expect(await screen.findByText(/已导出 200000 行/)).toBeInTheDocument()
    expect(screen.getByText(/已截断：上限 20 万行/)).toBeInTheDocument()
  })
})
