import { useEffect, useMemo, useState } from 'react'
import { EmptyState } from '../../components/EmptyState'
import { Panel, PanelHeader } from '../../components/Panel'
import { QuotaCard } from '../../components/QuotaCard'
import { StatCard } from '../../components/StatCard'
import { useAsyncResource, useCommand } from '../../lib/async'
import { formatCompact, formatPoints, formatYuan } from '../../lib/format'
import { invokeCmd } from '../../lib/ipc'
import type { CommandError } from '../../lib/ipc'
import { buildCloudModels, quotaErrorText } from '../../lib/quota'
import type { CostTrend, QuotaInfo, TrendRange, UsageSummary, UsageTotals } from '../../lib/types'
import { CostCurve } from './CostCurve'
import { ModelList } from './ModelList'
import { RecentRequests } from './RecentRequests'

interface DashboardProps {
  /** 本机用量/请求刷新计数（代理活动、账户切换）。 */
  dataRevision?: number
  /** 云端额度与 24h 费用曲线刷新计数（quota://updated）。 */
  quotaRevision?: number
}

/**
 * 概览页：真实额度（quota_get）+ 今日/本月用量 + 24h/30d 费用曲线 + 云端模型分布 + 最近请求。
 * 额度失败区分 403/404/网络错误并保留上次成功快照；用量失败单独提示，不伪造 0。
 *
 * 取数走 `useAsyncResource`：首次加载显示加载态，事件刷新只静默更新；
 * 24h 云端曲线只跟 `quotaRevision`（额度）走，不再被登录态事件带着打网关。
 */
export function Dashboard({ dataRevision = 0, quotaRevision = 0 }: DashboardProps) {
  const [trendRange, setTrendRange] = useState<TrendRange>('24h')

  const quotaResource = useAsyncResource(`quota:${quotaRevision}`, () =>
    invokeCmd<QuotaInfo | null>('quota_get', { force: false })
  )
  const summaryResource = useAsyncResource(`summary:${dataRevision}`, () =>
    invokeCmd<UsageSummary>('usage_summary')
  )
  // 24h 云端曲线跟 quotaRevision；30d 本机曲线跟 dataRevision（新请求会改变本机日汇总）。
  const trendResource = useAsyncResource(
    `trend:${trendRange}:${trendRange === '24h' ? quotaRevision : dataRevision}`,
    () => invokeCmd<CostTrend>('cost_trend_get', { range: trendRange })
  )

  // 「刷新额度」是用户显式操作：自己维护错误，避免和事件刷新的错误互相覆盖。
  const { run: runQuotaRefresh, running: refreshingQuota } =
    useCommand<QuotaInfo | null>('quota_get')
  const [refreshError, setRefreshError] = useState<CommandError | null>(null)

  useEffect(() => {
    // 资源拿到新快照后，上一次手动刷新的错误已经过期。
    setRefreshError(null)
  }, [quotaResource.data])

  async function handleRefreshQuota() {
    const outcome = await runQuotaRefresh({ force: true })
    if (outcome.status === 'ok') {
      // 直接替换资源数据：命令已经打了一次网关，不必再读一遍本地快照。
      quotaResource.mutate(outcome.data)
      setRefreshError(null)
    } else if (outcome.status === 'error') {
      setRefreshError(outcome.error)
    }
  }

  const quota = quotaResource.data
  const summary = summaryResource.data
  const trend = trendResource.data
  const quotaError = refreshError ?? quotaResource.error
  const summaryError = summaryResource.error
  const trendError = trendResource.error
  const today = summary?.today
  const cloudModels = useMemo(() => buildCloudModels(quota), [quota])
  const quotaStale = quotaError != null && quota != null
  // 还没有任何额度数据时，任何一次请求（首次或重试）都显示占位；已有数据则静默刷新。
  const quotaLoading = quota == null && (quotaResource.loading || quotaResource.refreshing)
  // 切换范围后旧曲线的 range 与新范围不一致：继续显示加载态，不把旧范围的曲线当成新范围。
  const trendLoading =
    trendError == null &&
    ((trend == null && (trendResource.loading || trendResource.refreshing)) ||
      (trend != null && trend.range !== trendRange))
  const monthTokenValue = quota?.total_tokens == null ? '—' : formatCompact(quota.total_tokens)
  const monthCostValue = quota?.usage_cost == null ? '—' : formatPoints(quota.usage_cost)
  const trendMeta =
    trendRange === '24h' ? '账号云端 · 最近 24 小时（积分）' : '本机代理 · 最近 30 天（¥）'

  return (
    <div className="stack">
      <QuotaCard
        quota={quota}
        loading={quotaLoading}
        errorText={quotaError ? quotaErrorText(quotaError, quota != null) : null}
        stale={quotaStale}
        onRefresh={() => void handleRefreshQuota()}
        refreshing={refreshingQuota}
      />

      {summaryError ? (
        <div className="inline-message-row">
          <p className="inline-message inline-message--error" role="alert">
            读取用量失败：{summaryError.raw}
          </p>
          <button
            type="button"
            className="btn btn--ghost btn--sm"
            onClick={() => summaryResource.reload()}
          >
            重试
          </button>
        </div>
      ) : null}

      <section className="stat-grid" aria-label="今日与本月用量">
        <StatCard
          label="今日 Token"
          value={statValue(today, summary !== null, summaryError, 'tokens')}
          accent
        />
        <StatCard
          label="今日费用"
          value={statValue(today, summary !== null, summaryError, 'cost')}
        />
        <StatCard label="本月 Token" value={quotaLoading ? '…' : monthTokenValue} />
        <StatCard
          label="本月消耗"
          value={quotaLoading ? '…' : monthCostValue}
          unit={quota?.usage_cost == null ? undefined : '积分'}
        />
      </section>

      <Panel>
        <PanelHeader title="费用曲线" meta={trendMeta}>
          <div className="segmented-control" role="group" aria-label="费用曲线时间范围">
            <button
              type="button"
              className={trendRange === '24h' ? 'segmented-button is-active' : 'segmented-button'}
              aria-pressed={trendRange === '24h'}
              onClick={() => setTrendRange('24h')}
            >
              24 小时
            </button>
            <button
              type="button"
              className={trendRange === '30d' ? 'segmented-button is-active' : 'segmented-button'}
              aria-pressed={trendRange === '30d'}
              onClick={() => setTrendRange('30d')}
            >
              30 天
            </button>
          </div>
        </PanelHeader>
        {trendLoading ? (
          <p className="trend-note">
            正在读取{trendRange === '24h' ? '云端' : '本机'}费用数据…
          </p>
        ) : trendError ? (
          <EmptyState
            title="费用曲线暂不可用"
            text={`${trendError.raw}；切换时间范围会重新读取。`}
          />
        ) : !trend || trend.points.length === 0 || (trend.total_cost === 0 && trend.peak_cost === 0) ? (
          <EmptyState
            title="暂无费用数据"
            text={
              trendRange === '24h'
                ? '云端还没有最近 24 小时的小时费用快照。'
                : '本机代理产生费用记录后，这里会显示最近 30 天的每日费用。'
            }
          />
        ) : (
          <CostCurve trend={trend} />
        )}
      </Panel>

      <Panel>
        <PanelHeader title="模型分布" meta="本月各模型 token 与积分消耗">
          <span className="panel-tag">
            {quotaLoading ? '加载中' : `${cloudModels.total} 个模型`}
          </span>
        </PanelHeader>
        {quotaLoading ? (
          <p className="trend-note">正在读取云端模型用量…</p>
        ) : quotaError && quota == null ? (
          <EmptyState title="模型分布暂不可用" text="云端月度用量读取失败；修复后会自动恢复。" />
        ) : cloudModels.items.length === 0 ? (
          <EmptyState title="本月暂无模型用量" text="账号云端还没有本月模型消耗数据。" />
        ) : (
          <ModelList models={cloudModels.items} />
        )}
      </Panel>

      <RecentRequests
        summary={summary}
        error={summaryError}
        loading={summaryResource.loading}
        refreshing={summaryResource.refreshing}
      />
    </div>
  )
}

function statValue(
  totals: UsageTotals | undefined,
  hasData: boolean,
  error: CommandError | null,
  kind: 'tokens' | 'cost'
): string {
  // 只有"还没有任何数据"时才显示占位；后台刷新继续显示旧值，不再闪成"…"。
  if (!hasData) return '…'
  if (error) return '—'
  if (!totals || totals.requests === 0) return '—'
  return kind === 'tokens' ? formatCompact(totals.total_tokens) : formatYuan(totals.cost_yuan)
}
