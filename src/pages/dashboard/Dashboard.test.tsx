// @vitest-environment jsdom
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { Dashboard } from './index'
import { invokeCmd } from '../../lib/ipc'
import { formatDateTime } from '../../lib/format'
import type { CostTrend } from '../../lib/types'
import { EMPTY_USAGE_SUMMARY, QUOTA } from '../../test/fixtures'

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  return { ...actual, isTauri: () => true, invokeCmd: vi.fn() }
})

const invokeMock = vi.mocked(invokeCmd)

const EMPTY_TREND: CostTrend = {
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

describe('Dashboard 失败分支', () => {
  it('summary_failure_shows_error_and_retry_instead_of_endless_loading', async () => {
    let failSummary = true
    invokeMock.mockImplementation(async (command) => {
      switch (command) {
        case 'quota_get':
          return QUOTA
        case 'usage_summary':
          if (failSummary) throw new Error('db locked')
          return EMPTY_USAGE_SUMMARY
        case 'cost_trend_get':
          return EMPTY_TREND
        default:
          throw new Error(`未 mock 的命令：${String(command)}`)
      }
    })

    render(<Dashboard />)

    // 错误分支优先：统计卡明确显示“—”，不能永远挂着“…”（旧 P2-5）。
    expect(await screen.findByText(/读取用量失败：db locked/)).toBeInTheDocument()
    const tokenCard = screen.getByText('今日 Token').closest('article')
    expect(tokenCard).toHaveTextContent('—')
    expect(tokenCard).not.toHaveTextContent('…')

    // “最近请求”同样先显示错误与重试入口，而不是“正在读取最近请求…”。
    expect(screen.getByText('最近请求暂不可用')).toBeInTheDocument()
    expect(screen.queryByText('正在读取最近请求…')).toBeNull()

    failSummary = false
    const retryButtons = screen.getAllByRole('button', { name: '重试' })
    fireEvent.click(retryButtons[retryButtons.length - 1])

    await waitFor(() => expect(screen.queryByText(/读取用量失败/)).toBeNull())
    expect(await screen.findByText('暂无请求记录')).toBeInTheDocument()
  })

  it('quota_failure_uses_alert_role', async () => {
    invokeMock.mockImplementation(async (command) => {
      switch (command) {
        case 'quota_get':
          throw new Error('上游返回 HTTP 403')
        case 'usage_summary':
          return EMPTY_USAGE_SUMMARY
        case 'cost_trend_get':
          return EMPTY_TREND
        default:
          throw new Error(`未 mock 的命令：${String(command)}`)
      }
    })

    render(<Dashboard />)
    const message = await screen.findByText(/无权限访问额度信息/)
    // N-P3-34：额度错误是告警，不是普通状态文案。
    expect(message).toHaveAttribute('role', 'alert')
  })

  it('cost_curve_uses_formatted_time_labels_and_keyboard_navigation', async () => {
    const trend: CostTrend = {
      range: '24h',
      source: 'cloud',
      unit: 'points',
      points: [
        { label: '10-25 09:00', at: '2026-10-25T01:00:00Z', cost: 1, tokens: 100 },
        { label: '10-25 10:00', at: '2026-10-25T02:00:00Z', cost: 2, tokens: 200 }
      ],
      total_cost: 3,
      peak_cost: 2,
      updated_at: null,
      generated_at: '2026-10-25T02:05:00Z',
      credit_to_yuan: 0.01
    }
    invokeMock.mockImplementation(async (command) => {
      switch (command) {
        case 'quota_get':
          return QUOTA
        case 'usage_summary':
          return EMPTY_USAGE_SUMMARY
        case 'cost_trend_get':
          return trend
        default:
          throw new Error(`未 mock 的命令：${String(command)}`)
      }
    })

    render(<Dashboard />)
    // 轴标签用后端 label（不再由前端按字符位置截 ISO 串）。
    expect(await screen.findByText('10-25 09:00')).toBeInTheDocument()
    expect(screen.getByText('10-25 10:00')).toBeInTheDocument()

    const plot = screen.getByRole('group', { name: /共 2 个点/ })
    expect(plot).toHaveAttribute('tabindex', '0')

    fireEvent.keyDown(plot, { key: 'ArrowRight' })
    const tooltip = await screen.findByRole('tooltip')
    expect(tooltip).toHaveAttribute('aria-live', 'polite')
    // tooltip 时间走全站 formatDateTime，而不是直出后端原始时间串（N-P3-33）。
    expect(within(tooltip).getByText(formatDateTime(trend.points[0].at))).toBeInTheDocument()
    expect(within(tooltip).queryByText(trend.points[0].at)).toBeNull()

    fireEvent.keyDown(plot, { key: 'ArrowRight' })
    expect(within(await screen.findByRole('tooltip')).getByText(formatDateTime(trend.points[1].at)))
      .toBeInTheDocument()

    // 边界不越界：再按一次仍停在最后一个点。
    fireEvent.keyDown(plot, { key: 'ArrowRight' })
    expect(within(screen.getByRole('tooltip')).getByText(formatDateTime(trend.points[1].at)))
      .toBeInTheDocument()
  })
})
