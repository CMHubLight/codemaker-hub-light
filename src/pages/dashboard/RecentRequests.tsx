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
  loading: boolean
  refreshing: boolean
}

/**
 * “最近请求”面板。
 *
 * `memo`：`summary/error/loading/refreshing` 都是稳定引用/原始值，
 * 概览页切换时间范围或悬停曲线时这个面板不重渲染。
 */
export const RecentRequests = memo(function RecentRequests({
  summary,
  error,
  loading,
  refreshing
}: RecentRequestsProps) {
  const recent = summary?.recent ?? []
  return (
    <Panel>
      <PanelHeader title="最近请求" meta="最新代理请求；完整筛选见“请求”页">
        <span className="panel-tag">
          {refreshing ? '刷新中…' : summary ? `${recent.length} 条` : loading ? '加载中' : '—'}
        </span>
      </PanelHeader>
      {summary === null ? (
        <p className="trend-note">正在读取最近请求…</p>
      ) : error ? (
        <EmptyState title="最近请求暂不可用" text="用量数据读取失败；修复后会自动恢复。" />
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
