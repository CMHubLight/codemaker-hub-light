import { memo } from 'react'
import { EmptyState } from '../../components/EmptyState'
import { Panel, PanelHeader } from '../../components/Panel'
import { StatusPill } from '../../components/StatusPill'
import { formatCompact, formatDateTime, formatYuan } from '../../lib/format'
import type { CommandError } from '../../lib/ipc'
import { protocolLabel, requestStatus } from '../../lib/labels'
import type { RecentRequest, UsageSummary } from '../../lib/types'

interface RecentRequestsProps {
  summary: UsageSummary | null
  error: CommandError | null
  /** `summary == null && loading` 才是“正在加载”；已有数据时后台刷新不闪加载态。 */
  loading: boolean
  refreshing: boolean
  /** 读取失败时的重试入口。 */
  onRetry: () => void
}

/**
 * “最近请求”面板。
 *
 * 错误分支优先于加载分支：旧实现先判 `summary === null`，首屏失败时永远显示
 * “正在读取最近请求…”，错误只藏在概览页顶部的小字里（旧 P2-5）。
 *
 * `memo`：`summary/error/loading/refreshing/onRetry` 都是稳定引用/原始值，
 * 概览页切换时间范围或悬停曲线时这个面板不重渲染。
 */
export const RecentRequests = memo(function RecentRequests({
  summary,
  error,
  loading,
  refreshing,
  onRetry
}: RecentRequestsProps) {
  const recent = summary?.recent ?? []
  return (
    <Panel>
      <PanelHeader title="最近请求" meta="最新代理请求；完整筛选见“请求”页">
        <span className="panel-tag">
          {error ? '读取失败' : refreshing ? '刷新中…' : summary ? `${recent.length} 条` : loading ? '加载中' : '—'}
        </span>
      </PanelHeader>
      {error ? (
        <EmptyState
          title="最近请求暂不可用"
          text={`用量数据读取失败：${error.raw}`}
          action={
            <button type="button" className="btn btn--ghost btn--sm" onClick={onRetry}>
              重试
            </button>
          }
        />
      ) : summary == null && loading ? (
        <p className="trend-note">正在读取最近请求…</p>
      ) : summary == null ? (
        <EmptyState title="最近请求暂不可用" text="用量数据尚未就绪；请稍后重试。" />
      ) : recent.length === 0 ? (
        <EmptyState
          title="暂无请求记录"
          text="把 Claude Code / Codex / OpenAI SDK 指向本地代理后，请求会显示在这里。"
        />
      ) : (
        <ul className="recent-list">
          {recent.slice(0, 8).map((item) => (
            <RecentRequestRow key={item.id} item={item} />
          ))}
        </ul>
      )}
    </Panel>
  )
})

/** 单行请求：`item` 引用稳定，列表刷新外的父级重渲染不重算每行。 */
const RecentRequestRow = memo(function RecentRequestRow({ item }: { item: RecentRequest }) {
  const status = requestStatus(item.status)
  return (
    <li className="recent-item">
      <span className="recent-time">{formatDateTime(item.created_at)}</span>
      <span className="recent-model mono" title={item.model}>
        {item.model}
      </span>
      <span className="recent-meta">{protocolLabel(item.protocol)}</span>
      <span className="recent-tokens">
        {formatCompact(item.input_tokens + item.output_tokens)} tokens
      </span>
      <span className="recent-cost mono">{formatYuan(item.cost_yuan)}</span>
      <span className="recent-status">
        <StatusPill status={status.tone} label={status.label} />
      </span>
    </li>
  )
})
