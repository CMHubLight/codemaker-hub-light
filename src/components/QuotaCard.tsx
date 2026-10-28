import { formatDateTime, formatPoints } from '../lib/format'
import type { QuotaInfo } from '../lib/types'
import { StatusPill } from './StatusPill'
import type { PillStatus } from './StatusPill'

interface QuotaCardProps {
  quota: QuotaInfo | null
  loading?: boolean
  /** 刷新失败时给用户看的文案；有上次数据时以“旧数据”降级展示。 */
  errorText?: string | null
  /** 当前展示的是上次成功快照（本次刷新失败）。 */
  stale?: boolean
  onRefresh?: () => void
  refreshing?: boolean
}

/**
 * 月度额度卡。额度失败时绝不用 0 伪造：
 * - 有上次快照 → 保留数值并标注“旧数据”；
 * - 无任何快照 → 显示“暂无数据 + 明确错误”。
 */
export function QuotaCard({
  quota,
  loading = false,
  errorText = null,
  stale = false,
  onRefresh,
  refreshing = false
}: QuotaCardProps) {
  if (loading) {
    return (
      <section className="quota-card" aria-busy="true">
        <div className="quota-head">
          <div>
            <p className="quota-eyebrow">月度额度</p>
            <p className="quota-remaining">…</p>
          </div>
          <StatusPill status="idle" label="加载中" />
        </div>
        <div className="quota-bar">
          <div className="quota-bar-fill quota-bar-fill--skeleton" style={{ width: '38%' }} />
        </div>
      </section>
    )
  }

  const percent = resolvePercent(quota)
  const status = resolveStatus(quota, errorText, stale)
  const hasQuota = quota != null

  return (
    <section className="quota-card">
      <div className="quota-head">
        <div>
          <p className="quota-eyebrow">
            月度额度{hasQuota ? ' · 剩余' : null}
          </p>
          <p className="quota-remaining">
            {quota?.remaining == null ? '—' : formatPoints(quota.remaining)}
            {quota?.remaining == null ? null : <span className="quota-unit">积分</span>}
          </p>
          <p className="quota-sub">
            {hasQuota
              ? `已用 ${formatPoints(quota.used)} · 总额 ${formatPoints(quota.monthly_quota)}`
              : '尚未获取额度数据'}
          </p>
        </div>

        <div className="quota-head-actions">
          <StatusPill status={status.status} label={status.label} />
          {onRefresh ? (
            <button
              type="button"
              className="btn btn--ghost btn--sm"
              onClick={onRefresh}
              disabled={refreshing}
            >
              {refreshing ? '刷新中…' : '刷新额度'}
            </button>
          ) : null}
        </div>
      </div>

      {percent == null ? (
        <div className="quota-bar">
          <div className="quota-bar-fill" style={{ width: '0%' }} />
        </div>
      ) : (
        <div
          className="quota-bar"
          role="progressbar"
          aria-label="额度使用进度"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(percent)}
        >
          <div
            className={percent >= 90 ? 'quota-bar-fill quota-bar-fill--danger' : 'quota-bar-fill'}
            style={{ width: `${percent}%` }}
          />
        </div>
      )}

      {hasQuota ? (
        <div className="quota-foot">
          <span>重置 {resolveReset(quota)}</span>
          <span>
            快照更新于 {formatDateTime(quota.fetched_at)}
            {quota.timezone ? ` · ${quota.timezone}` : ''}
          </span>
        </div>
      ) : null}

      {errorText ? (
        <p className={`inline-message inline-message--${stale ? 'warn' : 'error'}`} role="status">
          {errorText}
        </p>
      ) : !hasQuota ? (
        <p className="quota-empty">
          暂无额度数据：登录后会自动拉取；也可以点击“刷新额度”重试。
        </p>
      ) : null}
    </section>
  )
}

function resolveStatus(
  quota: QuotaInfo | null,
  errorText: string | null,
  stale: boolean
): { status: PillStatus; label: string } {
  if (errorText && quota == null) return { status: 'error', label: '获取失败' }
  if (stale && quota != null) return { status: 'warn', label: '旧数据' }
  if (quota == null) return { status: 'idle', label: '暂无数据' }
  // 后端快照状态优先：ok/stale/error 是上次网关刷新的真实结果，
  // 比 is_active（月度额度是否启用）更能说明“这份数据是否新鲜”。
  switch (quota.status) {
    case 'error':
      return { status: 'error', label: '获取失败' }
    case 'stale':
      return { status: 'warn', label: '旧数据' }
    case 'ok':
      if (quota.is_active) return { status: 'ok', label: '生效中' }
      return { status: 'warn', label: '已停用' }
    default:
      // 未知状态码（后端新增取值）时退回按 is_active 判断。
      return quota.is_active
        ? { status: 'ok', label: '生效中' }
        : { status: 'warn', label: '已停用' }
  }
}

function resolvePercent(quota: QuotaInfo | null): number | null {
  if (quota == null) return null
  if (quota.monthly_quota != null && quota.monthly_quota > 0 && quota.used != null) {
    return clamp((quota.used / quota.monthly_quota) * 100)
  }
  if (quota.percentage != null && Number.isFinite(quota.percentage)) {
    return clamp(quota.percentage)
  }
  return null
}

function resolveReset(quota: QuotaInfo | null): string {
  if (quota == null) return '—'
  if (quota.reset_day != null) return `每月 ${quota.reset_day} 日`
  if (quota.reset_at) return formatDateTime(quota.reset_at)
  return '—'
}

function clamp(value: number): number {
  return Math.min(100, Math.max(0, value))
}
