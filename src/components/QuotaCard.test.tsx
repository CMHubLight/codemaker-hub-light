// @vitest-environment jsdom
import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { QuotaCard } from './QuotaCard'
import { quotaFixture } from '../test/fixtures'

describe('QuotaCard', () => {
  it('quota_card_renders_fractional_points：积分按小数展示，不再四舍五入成整数', () => {
    render(
      <QuotaCard
        quota={quotaFixture({ remaining: 12345.6, used: 10.25, monthly_quota: 99999.9 })}
      />
    )

    // 旧实现 formatInteger 会显示 12,346 / 10 / 100,000；这里必须保留最多 2 位小数。
    expect(screen.getByText(/12,345\.6/)).toBeInTheDocument()
    expect(screen.getByText(/已用 10\.25 · 总额 99,999\.9/)).toBeInTheDocument()
  })

  it('quota_card_uses_backend_status：优先采用后端 quota.status', () => {
    const { rerender } = render(<QuotaCard quota={quotaFixture({ status: 'stale' })} />)
    expect(screen.getByText('旧数据')).toBeInTheDocument()

    rerender(<QuotaCard quota={quotaFixture({ status: 'error' })} />)
    expect(screen.getByText('获取失败')).toBeInTheDocument()

    // 后端 ok 时仍按月度额度是否启用展示，保持原有文案。
    rerender(<QuotaCard quota={quotaFixture({ status: 'ok', is_active: true })} />)
    expect(screen.getByText('生效中')).toBeInTheDocument()
  })
})
