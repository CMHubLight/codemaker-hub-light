import { memo, useEffect, useRef, useState } from 'react'
import { Panel, PanelHeader } from '../components/Panel'
import { StatusPill } from '../components/StatusPill'
import { useAsyncResource } from '../lib/async'
import { formatCompact, formatDateTime, formatDuration, formatYuan } from '../lib/format'
import { invokeCmd } from '../lib/ipc'
import { protocolLabel, requestStatus } from '../lib/labels'
import type { Cursor, RecentRequest, RequestFilter, RequestPage } from '../lib/types'

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

interface LoadedRequestPage {
  key: string
  page: RequestPage
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
  const pageResource = useAsyncResource<LoadedRequestPage>(queryKey, async () => ({
    key: queryKey,
    page: await invokeCmd<RequestPage>('usage_requests', {
      filter: toRequestFilter(applied, currentCursor, includeTotal)
    })
  }))
  // “查询中”派生自资源状态，不再用 state + effect 维护：
  // 当前 queryKey 还没有落地（成功或失败）的响应 = 用户刚改了筛选/翻页/重试；
  // 旧实现在重试时会被上一个请求的 data/error 变化提前清掉，按钮在真正开始取数前
  // 就恢复可点（P3-6）。`settledKey` 由 useAsyncResource 维护，失败也算落地。
  const applyingFilter = pageResource.settledKey !== queryKey
  const querying = pageResource.refreshing || applyingFilter

  // 第一页带回 total；后续页 total = -1 表示“未统计”，沿用已有总数。
  // 派生值优先用当前响应（同帧可见），ref 只作为后续页的兜底。
  const pageData = pageResource.data?.key === queryKey ? pageResource.data.page : null
  const pageTotal = pageData?.total
  const total = pageTotal != null && pageTotal >= 0 ? pageTotal : knownTotalRef.current
  useEffect(() => {
    if (pageTotal != null && pageTotal >= 0) knownTotalRef.current = pageTotal
  }, [pageTotal])

  // 事件刷新：本页是 keyset 快照，重取当前页即可。
  // 旧实现每次都 setPageIndex(0) + setCursors([null])，一次额度 tick 会把正在翻页的
  // 用户打回第 1 页（旧 P2-4）；只有停在第 1 页时才需要重置游标栈。
  useEffect(() => {
    if (revisionRef.current === revision) return
    revisionRef.current = revision
    if (pageIndex === 0) {
      // 第一页要重新统计 total（后台清理/新请求都可能改变总数）。
      knownTotalRef.current = null
      setCursors([null])
    }
    setReloadNonce((value) => value + 1)
  }, [pageIndex, revision])

  /** 筛选变化 / 重试 / 回第一页：游标栈失效，总数作废后从第一页（含 total）重取。 */
  function resetToFirstPage() {
    // 旧 total 属于上一个筛选条件：不清掉会让“共 N 条”和新列表一起显示（旧 P2-6）。
    knownTotalRef.current = null
    setPageIndex(0)
    setCursors([null])
    setReloadNonce((value) => value + 1)
  }

  function update<K extends keyof FilterState>(key: K, value: FilterState[K]) {
    setDraft((current) => ({ ...current, [key]: value }))
  }

  function applyFilters() {
    setApplied({ ...draft })
    resetToFirstPage()
  }

  function resetFilters() {
    setDraft(DEFAULT_FILTERS)
    setApplied(DEFAULT_FILTERS)
    resetToFirstPage()
  }

  function nextPage() {
    // 回退过的页游标还在栈里：前进优先复用，避免再依赖当前页的 next_cursor。
    const next = cursors[pageIndex + 1] ?? pageData?.next_cursor ?? null
    if (!next) return
    setCursors((current) => [...current.slice(0, pageIndex + 1), next])
    setPageIndex(pageIndex + 1)
  }

  function prevPage() {
    if (pageIndex === 0) return
    setPageIndex(pageIndex - 1)
  }

  function retry() {
    resetToFirstPage()
  }

  const error = pageResource.error
  const loading = pageResource.loading || querying
  const items = pageData?.items ?? []
  const totalCount = total ?? 0
  // `next_cursor` 是后端“还有没有下一页”的唯一权威（多取一行前瞻，满页不再等于有下一页）；
  // total 只在第一页统计，用来补足总页数显示，不能反过来决定能不能翻页（旧 P2-6）。
  const canNext = pageData?.next_cursor != null
  const knownPages = total != null ? Math.ceil(Math.max(total, 0) / PAGE_SIZE) : 0
  const totalPages = Math.max(knownPages, pageIndex + 1 + (canNext ? 1 : 0), 1)
  const filtersPending = !filtersEqual(draft, applied)
  const filterActive =
    applied.protocol !== '' ||
    applied.status !== '' ||
    applied.keyword.trim() !== '' ||
    applied.range !== DEFAULT_FILTERS.range
  // 后端多取一行后，整数倍满页不会再给出空页；但数据被清理/并发删除时仍可能取到空页，
  // 此时给出“本页暂无数据 + 回到第一页”，而不是让人以为筛选坏了（旧 P2-6）。
  const emptyLaterPage = pageIndex > 0 && error == null && !loading
  const emptyTitle = error
    ? '请求列表加载失败'
    : loading
      ? '正在加载请求…'
      : emptyLaterPage
        ? '本页暂无数据'
        : filterActive
          ? '当前筛选条件下没有请求'
          : '暂无请求记录'
  const emptyText = error
    ? `无法读取本地用量库：${error.raw}`
    : loading
      ? '正在从本地用量库读取 usage_requests。'
      : emptyLaterPage
        ? '这一页的请求可能已被清理；回到第一页可查看最新记录。'
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
          </div>
        </div>
        <p className="panel-meta">
          筛选与分页在本地用量库执行；请求列表暂未关联会话记录，客户端列固定显示 unknown。
        </p>
        {filtersPending ? (
          <p className="panel-meta" role="status">
            筛选未应用：当前列表仍按上次查询条件，点击“查询”后生效。
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
          <span className="panel-tag">{loading ? '查询中…' : `共 ${totalCount} 条`}</span>
        </PanelHeader>

        <div className="table-wrap">
          <table className="requests-table">
            <caption className="visually-hidden">请求明细：按时间倒序，支持时间范围、协议、状态与模型关键字筛选</caption>
            <thead>
              <tr>
                <th scope="col">时间</th>
                <th scope="col">模型</th>
                <th scope="col">协议</th>
                <th scope="col">客户端</th>
                <th scope="col" className="num">输入</th>
                <th scope="col" className="num">输出</th>
                <th scope="col" className="num">费用</th>
                <th scope="col" className="num">首字节</th>
                <th scope="col" className="num">耗时</th>
                <th scope="col">状态</th>
              </tr>
            </thead>
            <tbody>
              {items.length === 0 ? (
                <tr>
                  <td colSpan={10} className="empty-cell">
                    <p className="empty-state-title">{emptyTitle}</p>
                    <p className="empty-state-text">{emptyText}</p>
                    {emptyLaterPage ? (
                      <div className="empty-state-actions">
                        <button type="button" className="btn btn--ghost btn--sm" onClick={retry}>
                          回到第一页
                        </button>
                      </div>
                    ) : null}
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
