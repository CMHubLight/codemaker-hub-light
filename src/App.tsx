import { useCallback, useEffect, useState } from 'react'
import { ErrorBoundary } from './components/ErrorBoundary'
import { useCoalescedRefresh, useCommand } from './lib/async'
import { useTauriEvent } from './lib/events'
import { formatDateTime } from './lib/format'
import { invokeCmd, isTauri } from './lib/ipc'
import type { AuthStatus } from './lib/types'
import { Dashboard } from './pages/dashboard'
import { Login } from './pages/Login'
import { Requests } from './pages/Requests'
import { Settings } from './pages/settings'

type PageKey = 'dashboard' | 'requests' | 'settings'

interface PageMeta {
  key: PageKey
  label: string
  description: string
}

const PAGES: PageMeta[] = [
  { key: 'dashboard', label: '概览', description: '月度额度、今日/本月用量、费用曲线与最近请求' },
  { key: 'requests', label: '请求', description: '逐请求 token、费用与耗时明细' },
  { key: 'settings', label: '设置', description: '代理、记录策略与客户端接入说明' }
]

const UNAUTHENTICATED: AuthStatus = {
  state: 'unauthenticated',
  user: null,
  display_name: null,
  expires_at: null,
  offline_grace: false,
  message: null,
  dept: null,
  server_url: null,
  issued_at: null,
  credentials_valid: false,
  pending_poll_uuid: null,
  pending_login_started_at: null,
  deep_link_received: false,
  last_validated_at: null
}

/**
 * 应用外壳：未登录时全屏登录页；已登录时左侧栏 + 页面切换（不引入路由库）。
 *
 * 数据刷新：订阅后端 `auth://state`、`auth://callback`、`quota://updated`、
 * `proxy://status`、`proxy://error`、`app://second-instance`。所有事件先经过
 * `useCoalescedRefresh` 合并，再拆成两个互不影响的 revision：
 * - `dataRevision`：本机用量/请求（随代理活动、账户切换变化）；
 * - `quotaRevision`：云端额度与 24h 费用曲线（由 `quota://updated` 驱动）。
 * 这样一次后台 tick 连发多个事件只触发一轮重取，登录态事件也不会带着概览页
 * 重新请求一次 24h 云端费用（网关 HTTP）。
 *
 * 浏览器预览（无 Tauri）下事件订阅自动跳过，走 mock 数据。
 */
export default function App() {
  const [page, setPage] = useState<PageKey>('dashboard')
  const [auth, setAuth] = useState<AuthStatus | null>(null)
  const [authLoading, setAuthLoading] = useState(true)
  const [authError, setAuthError] = useState<string | null>(null)
  /** 只有 `auth_status` 本身失败（含超时）才算"连不上后端"，与登出等操作错误分开。 */
  const [bootFailed, setBootFailed] = useState(false)
  /** 浏览器预览（无 Tauri 后端）时允许跳过登录查看界面；仅内存态。 */
  const [previewSignedIn, setPreviewSignedIn] = useState(false)

  const { revision: dataRevision, bump: bumpData } = useCoalescedRefresh(50)
  const { revision: quotaRevision, bump: bumpQuota } = useCoalescedRefresh(50)
  const { run: runAuthStatus } = useCommand<AuthStatus>('auth_status')
  const { run: runLogout } = useCommand<AuthStatus>('auth_logout')

  const previewMode = !isTauri()

  const loadAuth = useCallback(async () => {
    const outcome = await runAuthStatus()
    // 已有更新的加载在跑（或组件已卸载）：本次结果作废，由最新调用收尾。
    if (outcome.status === 'stale') return
    if (outcome.status === 'ok') {
      setAuth(outcome.data)
      setAuthError(null)
      setBootFailed(false)
    } else {
      setAuthError(outcome.error.raw)
      setBootFailed(true)
      setAuth({ ...UNAUTHENTICATED, state: 'error', message: outcome.error.raw })
    }
    setAuthLoading(false)
  }, [runAuthStatus])

  useEffect(() => {
    void loadAuth()
  }, [loadAuth])

  // 登录态事件：后端只在状态真的变化时广播，直接更新界面即可。
  // 用量/额度数据不在这里重取：账户变化时后端会补发 quota://updated，
  // 避免 validate 类事件带着概览/请求页整页重拉。
  useTauriEvent('auth://state', (status) => {
    setAuth(status)
    setAuthLoading(false)
    setAuthError(null)
    setBootFailed(false)
    if (status.state === 'unauthenticated') {
      setPreviewSignedIn(false)
      setPage('dashboard')
    }
  })

  // 深链到达：可能已带回凭据，重新读取一次状态（后端同时会发 auth://state）。
  useTauriEvent('auth://callback', () => {
    void loadAuth()
  })

  // 额度快照更新：额度卡与 24h 费用曲线跟随 quotaRevision；
  // 本机用量/请求也顺带刷新（本地读库，不打网关），保持原来 5 分钟一次的后台刷新节奏。
  useTauriEvent('quota://updated', () => {
    bumpQuota()
    bumpData()
  })

  // 代理启动/重启/端口回退：刷新设置页代理状态与本机请求计数。
  useTauriEvent('proxy://status', bumpData)
  useTauriEvent('proxy://error', bumpData)

  // 重复启动（后端已聚焦窗口）：刷新登录态与全部数据，避免两个入口看到不同状态。
  useTauriEvent('app://second-instance', () => {
    void loadAuth()
    bumpData()
    bumpQuota()
  })

  const authenticated = previewSignedIn || auth?.state === 'authenticated'

  const retryAuth = useCallback(() => {
    setAuthError(null)
    setBootFailed(false)
    setAuthLoading(true)
    void loadAuth()
  }, [loadAuth])

  function openLogs() {
    void invokeCmd<unknown>('app_open_logs').catch(() => {
      // 后端不可用时打开日志也会失败；失败屏本身已给出日志路径。
    })
  }

  async function handleLogout() {
    const outcome = await runLogout()
    if (outcome.status === 'error') {
      // 即使命令失败也回到未登录界面，避免把用户困在已登录态。
      setAuthError(outcome.error.raw)
    }
    setPreviewSignedIn(false)
    setAuth(UNAUTHENTICATED)
    setPage('dashboard')
    bumpData()
    bumpQuota()
  }

  if (authLoading) {
    return (
      <div className="boot-screen">
        <span className="brand-mark brand-mark--lg">CH</span>
        <p>正在连接 CodeMaker Hub Light…</p>
      </div>
    )
  }

  // 启动期命令失败（含 10s 超时）：给出失败屏和重试入口，
  // 不再永远停在"正在连接…"。
  if (!authenticated && bootFailed) {
    return (
      <div className="boot-screen boot-screen--error" role="alert">
        <span className="brand-mark brand-mark--lg">CH</span>
        <h1>无法连接本地后端</h1>
        <p>{authError}</p>
        <p className="boot-hint">
          日志目录：%USERPROFILE%\.codemaker-hub-light\logs\；详细原因见 hub-light.log 与
          startup-error.log。
        </p>
        <div className="boot-actions">
          <button type="button" className="btn btn--primary" onClick={retryAuth}>
            重试连接
          </button>
          <button type="button" className="btn btn--ghost" onClick={openLogs}>
            打开日志目录
          </button>
        </div>
      </div>
    )
  }

  if (!authenticated) {
    return (
      <Login
        previewMode={previewMode}
        initialStatus={auth}
        onAuthenticated={loadAuth}
        onPreviewSignIn={() => setPreviewSignedIn(true)}
      />
    )
  }

  const currentPage = PAGES.find((item) => item.key === page) ?? PAGES[0]
  const realAccount = auth?.state === 'authenticated'
  // 左下角只展示登录用户名（如 alice），不再优先使用 display_name。
  const accountName = realAccount ? (auth.user ?? auth.display_name) : '预览账号'
  const accountExpiry =
    realAccount && auth.expires_at
      ? `${auth.offline_grace ? '离线宽限 · ' : ''}有效期至 ${formatDateTime(auth.expires_at)}`
      : null

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-mark">CH</span>
          <div className="brand-text">
            <p className="brand-name">CodeMaker Hub Light</p>
          </div>
        </div>

        <nav className="nav" aria-label="主导航">
          {PAGES.map((item) => (
            <button
              key={item.key}
              type="button"
              className={item.key === page ? 'nav-item is-active' : 'nav-item'}
              aria-current={item.key === page ? 'page' : undefined}
              onClick={() => setPage(item.key)}
            >
              <span className="nav-icon" aria-hidden="true">
                <NavIcon page={item.key} />
              </span>
              <span>{item.label}</span>
            </button>
          ))}
        </nav>

        <div className="sidebar-footer">
          {accountName ? <p className="account-name">{accountName}</p> : null}
          {accountExpiry ? <p className="account-meta">{accountExpiry}</p> : null}
        </div>
      </aside>

      <main className="main">
        <header className={previewMode ? 'page-header page-header--preview' : 'page-header'}>
          <div>
            <h1>{currentPage.label}</h1>
            <p>{currentPage.description}</p>
          </div>
          {previewMode ? (
            <span className="env-badge env-badge--preview">浏览器预览 · mock IPC</span>
          ) : null}
        </header>

        {authError ? (
          <p className="inline-message inline-message--error" role="alert">
            登录状态异常：{authError}
          </p>
        ) : null}

        {page === 'dashboard' ? (
          <ErrorBoundary
            key="dashboard"
            scope="概览"
            onReset={() => {
              bumpData()
              bumpQuota()
            }}
          >
            <Dashboard dataRevision={dataRevision} quotaRevision={quotaRevision} />
          </ErrorBoundary>
        ) : null}
        {page === 'requests' ? (
          <ErrorBoundary key="requests" scope="请求" onReset={bumpData}>
            <Requests revision={dataRevision} />
          </ErrorBoundary>
        ) : null}
        {page === 'settings' ? (
          <ErrorBoundary
            key="settings"
            scope="设置"
            onReset={() => {
              bumpData()
              bumpQuota()
            }}
          >
            <Settings auth={auth} revision={dataRevision} onLogout={handleLogout} />
          </ErrorBoundary>
        ) : null}
      </main>
    </div>
  )
}

/** 侧边导航图标：内联 SVG，避免为三个图标引入图标库。 */
function NavIcon({ page }: { page: PageKey }) {
  if (page === 'dashboard') {
    return (
      <svg viewBox="0 0 20 20">
        <rect x="3" y="3" width="6" height="6" rx="1.5" />
        <rect x="11" y="3" width="6" height="6" rx="1.5" />
        <rect x="3" y="11" width="6" height="6" rx="1.5" />
        <rect x="11" y="11" width="6" height="6" rx="1.5" />
      </svg>
    )
  }

  if (page === 'requests') {
    return (
      <svg viewBox="0 0 20 20">
        <path d="M4 5.5h12M4 10h12M4 14.5h8" />
      </svg>
    )
  }

  return (
    <svg viewBox="0 0 20 20">
      <path d="M4 6.5h12M4 13.5h12" />
      <circle cx="8" cy="6.5" r="2" />
      <circle cx="12" cy="13.5" r="2" />
    </svg>
  )
}
