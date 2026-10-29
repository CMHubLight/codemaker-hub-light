import { memo, useEffect, useRef, useState } from 'react'
import { Panel, PanelHeader } from '../components/Panel'
import { StatusPill } from '../components/StatusPill'
import { useAsyncResource, useCommand } from '../lib/async'
import { formatCompact, formatDateTime, formatDuration, formatYuan } from '../lib/format'
import { invokeCmd } from '../lib/ipc'
import { protocolLabel, requestStatus } from '../lib/labels'
import type {
  Cursor,
  RecentRequest,
  RequestFilter,
  RequestPage,
  UsageExportResult
} from '../lib/types'

interface FilterState {
  range: string
  protocol: string
  status: string
  keyword: string
}

const DEFAULT_FILTERS: FilterState = {
  range: '7d',
  protocol: '',
  status: '',
  keyword: ''
}

const PAGE_SIZE = 50

const RANGE_OPTIONS = [
  { value: 'today', label: '今天' },
  { value: '7d', label: '近 7 天' },
  { value: '30d', label: '近 30 天' },
  { value: 'all', label: '全部' }
]

const PROTOCOL_OPTIONS = [
  { value: '', label: '全部协议' },
  { value: 'anthropic', label: 'Anthropic Messages' },
  { value: 'openai_chat', label: 'OpenAI Chat' },
  { value: 'openai_responses', label: 'OpenAI Responses' }
]

const STATUS_OPTIONS = [
  { value: '', label: '全部状态' },
  { value: 'success', label: '成功' },
  { value: 'error', label: '失败' },
  { value: 'cancelled', label: '已取消' },
  { value: 'unknown', label: '未知' }
]

interface RequestsProps {
  /** App 在用量/请求数据变化后递增（已合并窗口内的多个事件）。 */
  revision?: number
}

/**
 * 请求页：在本地用量库上做真实筛选与分页（usage_requests）。
 * 错误时保留上次结果并提示重试，不用空表覆盖。
 */
export function Requests({ revision = 0 }: RequestsProps) {
  const [draft, setDraft] = useState<FilterState>(DEFAULT_FILTERS)
  const [applied, setApplied] = useState<FilterState>(DEFAULT_FILTERS)
  /**
   * keyset 游标栈：第 0 页固定为 null；「下一页」把本页 `next_cursor` 入栈，
   * 「上一页」只回退下标（栈保留已访问页的游标，再前进时可直接复用）。
   */
  const [cursors, setCursors] = useState<(Cursor | null)[]>([null])
  const [pageIndex, setPageIndex] = useState(0)
  /**
   * 已知总条数：第一页统计后记住，后续页 `include_total=false` 返回 -1 时沿用。
   * 用 ref 而不是 state：总数只随第一页响应变化，派生读取可以避免
   * “行已渲染、总数还是旧值”的额外一帧（测试与用户都会看到闪变）。
   */
  const knownTotalRef = useRef<number | null>(null)
  const [reloadNonce, setReloadNonce] = useState(0)
  /** 用户主动查询（改筛选/翻页/重试）时置位：只有它显示阻塞式“查询中…”。 */
  const [querying, setQuerying] = useState(false)
  const [exportMessage, setExportMessage] = useState<string | null>(null)
  const revisionRef = useRef(revision)

  const currentCursor = cursors[pageIndex] ?? null
  const includeTotal = pageIndex === 0
  // key 里带上游标：同一条件下不同页是不同的资源；revision 不进 key，
  // 由下面的 effect 重置到第一页后统一重取，避免事件刷新先多请求一次旧页。
  const queryKey = JSON.stringify({
    from: applied.range,
    protocol: applied.protocol,
    status: applied.status,
    keyword: applied.keyword,
    after: currentCursor,
    page: pageIndex,
    nonce: reloadNonce
  })
  const pageResource = useAsyncResource(queryKey, () =>
    invokeCmd<RequestPage>('usage_requests', {
      filter: toRequestFilter(applied, currentCursor, includeTotal)
    })
  )
  const { run: runExport, running: exporting } = useCommand<UsageExportResult>('usage_export_csv')

  // 用户查询的结果（成功或失败）落地后收起“查询中…”。
  useEffect(() => {
    setQuerying(false)
  }, [pageResource.data, pageResource.error])

  // 第一页带回 total；后续页 total = -1 表示“未统计”，沿用已有总数。
  // 派生值优先用当前响应（同帧可见），ref 只作为后续页的兜底。
  const pageTotal = pageResource.data?.total
  const total = pageTotal != null && pageTotal >= 0 ? pageTotal : knownTotalRef.current
  useEffect(() => {
    if (pageTotal != null && pageTotal >= 0) knownTotalRef.current = pageTotal
  }, [pageTotal])

  // 事件刷新：keyset 页是快照，停在旧页会看到过期内容——静默回到第一页重取。
  useEffect(() => {
    if (revisionRef.current === revision) return
    revisionRef.current = revision
    setPageIndex(0)
    setCursors([null])
    setReloadNonce((value) => value + 1)
  }, [revision])

  /** 筛选变化 / 重试 / 事件刷新：游标栈失效，回到第一页（含 total）重取。 */
  function resetToFirstPage() {
    setPageIndex(0)
    setCursors([null])
    setReloadNonce((value) => value + 1)
  }

  function update<K extends keyof FilterState>(key: K, value: FilterState[K]) {
    setDraft((current) => ({ ...current, [key]: value }))
  }

  function applyFilters() {
    setQuerying(true)
    setApplied({ ...draft })
    resetToFirstPage()
  }

  function resetFilters() {
    setQuerying(true)
    setDraft(DEFAULT_FILTERS)
    setApplied(DEFAULT_FILTERS)
    resetToFirstPage()
  }

  function nextPage() {
    // 回退过的页游标还在栈里：前进优先复用，避免再依赖当前页的 next_cursor。
    const next = cursors[pageIndex + 1] ?? pageResource.data?.next_cursor ?? null
    if (!next) return
    setQuerying(true)
    setCursors((current) => [...current.slice(0, pageIndex + 1), next])
    setPageIndex(pageIndex + 1)
  }

  function prevPage() {
    if (pageIndex === 0) return
    setQuerying(true)
    setPageIndex(pageIndex - 1)
  }

  function retry() {
    setQuerying(true)
    resetToFirstPage()
  }

  async function exportCsv() {
    const filter = toRequestFilter(applied, null, false)
    setExportMessage(null)
    const outcome = await runExport({
      // 命令顶层参数名必须是 camelCase（Rust `from_day` → JS `fromDay`）；
      // 写成 snake_case 不会报错，只会被当成缺失、静默退回默认区间。
      fromDay: filter.from,
      toDay: filter.to
    })
    if (outcome.status === 'ok') {
      const result = outcome.data
      setExportMessage(
        result.saved
          ? `已导出 ${result.rows ?? 0} 行到 ${result.path ?? '本地 exports 目录'}${
              result.truncated ? '（已截断：上限 20 万行，请缩小时间范围）' : ''
            }`
          : (result.message ?? '导出失败')
      )
    } else if (outcome.status === 'error') {
      setExportMessage(`导出失败：${outcome.error.raw}`)
    }
  }

  const pageData = pageResource.data
  const error = pageResource.error
  const loading = pageResource.loading || querying
  const refreshing = pageResource.refreshing && !loading
  const items = pageData?.items ?? []
  const totalCount = total ?? 0
  const totalPages = Math.max(1, Math.ceil(totalCount / PAGE_SIZE))
  const canNext = pageIndex + 1 < cursors.length || pageData?.next_cursor != null
  const filtersPending = !filtersEqual(draft, applied)
  const filterActive =
    applied.protocol !== '' ||
    applied.status !== '' ||
    applied.keyword.trim() !== '' ||
    applied.range !== DEFAULT_FILTERS.range
  const emptyTitle = loading
    ? '正在加载请求…'
    : error
      ? '请求列表加载失败'
      : filterActive
        ? '当前筛选条件下没有请求'
        : '暂无请求记录'
  const emptyText = loading
    ? '正在从本地用量库读取 usage_requests。'
    : error
      ? `无法读取本地用量库：${error.raw}`
      : filterActive
        ? '可以放宽时间范围或清空筛选条件后重试。'
        : '把 Claude Code / Codex / OpenAI SDK 指向本地代理后，请求会记录在这里。'

  return (
    <div className="stack">
      <Panel>
        <div className="filter-row">
          <label className="field">
            <span>时间范围</span>
            <select value={draft.range} onChange={(event) => update('range', event.target.value)}>
              {RANGE_OPTIONS.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>

          <label className="field">
            <span>协议</span>
            <select value={draft.protocol} onChange={(event) => update('protocol', event.target.value)}>
              {PROTOCOL_OPTIONS.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>

          <label className="field">
            <span>状态</span>
            <select value={draft.status} onChange={(event) => update('status', event.target.value)}>
              {STATUS_OPTIONS.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>

          <label className="field field--grow">
            <span>模型关键字</span>
            <input
              type="text"
              value={draft.keyword}
              placeholder="例如 claude-sonnet-4"
              onChange={(event) => update('keyword', event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter') applyFilters()
              }}
            />
          </label>

          <div className="filter-actions">
            <button type="button" className="btn btn--primary" onClick={applyFilters} disabled={loading}>
              {loading ? '查询中…' : '查询'}
            </button>
            <button type="button" className="btn btn--ghost" onClick={resetFilters} disabled={loading}>
              重置
            </button>
            <button
              type="button"
              className="btn btn--ghost"
              onClick={() => void exportCsv()}
              disabled={exporting || loading}
            >
              {exporting ? '导出中…' : '导出 CSV'}
            </button>
          </div>
        </div>
        <p className="panel-meta">
          筛选与分页在本地用量库执行；请求列表暂未关联会话记录，客户端列固定显示 unknown。
          导出的 CSV 只按时间范围，不受协议/状态/关键字筛选影响。
        </p>
        {filtersPending ? (
          <p className="panel-meta" role="status">
            筛选未应用：当前列表仍按上次查询条件，点击“查询”后生效。
          </p>
        ) : null}
        {exportMessage ? (
          <p className="panel-meta" role="status">
            {exportMessage}
          </p>
        ) : null}
      </Panel>

      {error ? (
        <div className="inline-message-row">
          <p className="inline-message inline-message--error" role="alert">
            读取请求列表失败：{error.raw}
          </p>
          <button type="button" className="btn btn--ghost btn--sm" onClick={retry}>
            重试
          </button>
        </div>
      ) : null}

      <Panel>
        <PanelHeader title="请求明细" meta="逐请求 token 四元组、费用、首字节与总耗时">
          <span className="panel-tag">
            {loading ? '查询中…' : refreshing ? '刷新中…' : `共 ${totalCount} 条`}
          </span>
        </PanelHeader>

        <div className="table-wrap">
          <table className="requests-table">
            <thead>
              <tr>
                <th>时间</th>
                <th>模型</th>
                <th>协议</th>
                <th>客户端</th>
                <th className="num">输入</th>
                <th className="num">输出</th>
                <th className="num">费用</th>
                <th className="num">首字节</th>
                <th className="num">耗时</th>
                <th>状态</th>
              </tr>
            </thead>
            <tbody>
              {items.length === 0 ? (
                <tr>
                  <td colSpan={10} className="empty-cell">
                    <p className="empty-state-title">{emptyTitle}</p>
                    <p className="empty-state-text">{emptyText}</p>
                  </td>
                </tr>
              ) : (
                items.map((item) => <RequestRow key={item.id} item={item} />)
              )}
            </tbody>
          </table>
        </div>

        <footer className="table-footer">
          <span>
            共 {totalCount} 条 · 每页 {PAGE_SIZE} 条
          </span>
          <div className="pager">
            <button
              type="button"
              className="btn btn--ghost btn--sm"
              onClick={prevPage}
              disabled={loading || pageIndex === 0}
            >
              上一页
            </button>
            <span>
              第 {pageIndex + 1} / {totalPages} 页
            </span>
            <button
              type="button"
              className="btn btn--ghost btn--sm"
              onClick={nextPage}
              disabled={loading || !canNext}
            >
              下一页
            </button>
          </div>
        </footer>
      </Panel>
    </div>
  )
}

/** 单行请求：`item` 引用来自当前页数据，筛选输入等父级状态变化不重渲染已有行。 */
const RequestRow = memo(function RequestRow({ item }: { item: RecentRequest }) {
  const status = requestStatus(item.status)
  return (
    <tr>
      <td className="mono">{formatDateTime(item.created_at)}</td>
      <td className="mono">{item.model}</td>
      <td>{protocolLabel(item.protocol)}</td>
      <td>{item.client || '—'}</td>
      <td className="num">{formatCompact(item.input_tokens)}</td>
      <td className="num">{formatCompact(item.output_tokens)}</td>
      <td className="num mono">{formatYuan(item.cost_yuan)}</td>
      <td className="num">{formatDuration(item.first_byte_ms)}</td>
      <td className="num">{formatDuration(item.latency_ms)}</td>
      <td>
        <StatusPill status={status.tone} label={status.label} />
      </td>
    </tr>
  )
})

/** 两个筛选状态是否等价（用于提示“筛选未应用”：表单 draft 与已查询 applied 的比较）。 */
function filtersEqual(left: FilterState, right: FilterState): boolean {
  return (
    left.range === right.range &&
    left.protocol === right.protocol &&
    left.status === right.status &&
    left.keyword === right.keyword
  )
}

/**
 * 筛选状态 → usage_requests 的 RequestFilter（后端按日期前 10 位取日期）。
 * 分页用 keyset 游标：`after` 为 null 表示第一页；`include_total` 只在第一页
 * 请求 COUNT（后续页为 -1，省掉每页的 COUNT(*)）。
 */
function toRequestFilter(
  filters: FilterState,
  after: Cursor | null,
  includeTotal: boolean
): RequestFilter {
  const now = new Date()
  // 用本地日期而不是 toISOString()：后者按 UTC，东八区凌晨会落到前一天。
  const day = (date: Date) => {
    const year = date.getFullYear()
    const month = `${date.getMonth() + 1}`.padStart(2, '0')
    const dateOfMonth = `${date.getDate()}`.padStart(2, '0')
    return `${year}-${month}-${dateOfMonth}`
  }

  const from =
    filters.range === 'today'
      ? day(now)
      : filters.range === '7d'
        ? day(new Date(now.getFullYear(), now.getMonth(), now.getDate() - 6))
        : filters.range === '30d'
          ? day(new Date(now.getFullYear(), now.getMonth(), now.getDate() - 29))
          : undefined

  const keyword = filters.keyword.trim()

  return {
    from,
    to: filters.range === 'all' ? undefined : day(now),
    protocol: filters.protocol || undefined,
    status: filters.status || undefined,
    model: keyword || undefined,
    limit: PAGE_SIZE,
    after: after ?? undefined,
    include_total: includeTotal
  }
}
