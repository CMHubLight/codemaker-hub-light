import { useCallback, useEffect, useRef, useState } from 'react'
import { useCommand } from '../lib/async'
import { useTauriEvent } from '../lib/events'
import type { AuthStatus, LoginSession } from '../lib/types'

type LoginPhase = 'idle' | 'starting' | 'waiting' | 'error'

const DEFAULT_POLL_INTERVAL_MS = 2000
const DEFAULT_TIMEOUT_MS = 5 * 60 * 1000
/** 连续轮询命令失败达到该次数才判定登录失败（与后端 MAX_CONSECUTIVE_NETWORK_ERRORS 一致）。 */
const MAX_POLL_FAILURES = 3

interface LoginProps {
  /** 浏览器预览（无 Tauri 后端）时展示“跳过登录”入口。 */
  previewMode: boolean
  /** App 启动时已有的登录态：pending 直接进入等待，reauth_required/error 显示原因。 */
  initialStatus?: AuthStatus | null
  /** 真实登录成功后的回调。 */
  onAuthenticated: () => Promise<void> | void
  /** 预览模式下的临时登录，仅内存态。 */
  onPreviewSignIn: () => void
}

/**
 * 全屏登录页。流程（docs/authentication.md）：
 * auth_start_login 由后端唤起系统浏览器 → 前端每 2s 轮询 auth_poll_status，
 * 同时监听 `auth://state` / `auth://callback`：深链或轮询任一先成功即完成登录。
 */
export function Login({ previewMode, initialStatus = null, onAuthenticated, onPreviewSignIn }: LoginProps) {
  const [phase, setPhase] = useState<LoginPhase>(() => {
    if (initialStatus?.state === 'pending') return 'waiting'
    if (initialStatus?.state === 'reauth_required' || initialStatus?.state === 'error') return 'error'
    return 'idle'
  })
  const [message, setMessage] = useState<string | null>(() => {
    if (initialStatus?.state === 'reauth_required' || initialStatus?.state === 'error') {
      return initialStatus.message ?? '登录状态已失效，请重新登录。'
    }
    return null
  })
  /** 非致命提示（轮询重试、浏览器未自动打开等）。 */
  const [notice, setNotice] = useState<string | null>(null)
  const [session, setSession] = useState<LoginSession | null>(null)
  const [copyState, setCopyState] = useState<'idle' | 'copied' | 'failed'>('idle')
  const [manualUrl, setManualUrl] = useState('')
  const [manualBusy, setManualBusy] = useState(false)

  const startedAt = useRef<number | null>(null)
  const pollFailures = useRef(0)
  const finished = useRef(false)
  const pollNowRef = useRef<(() => void) | null>(null)
  // 登录相关的三条命令各自一个 runner：最新调用获胜 + 卸载后结果作废 + 错误归一。
  const { run: runPollStatus } = useCommand<AuthStatus>('auth_poll_status')
  const { run: runStartLogin } = useCommand<LoginSession>('auth_start_login')
  const { run: runSubmitDeepLink } = useCommand<AuthStatus>('auth_submit_deep_link')

  const finishLogin = useCallback(async () => {
    if (finished.current) return
    finished.current = true
    startedAt.current = null
    setPhase('idle')
    setNotice(null)
    await onAuthenticated()
  }, [onAuthenticated])

  // 事件优先：深链/后台轮询完成登录时立即切换，不必等下一次 2s 轮询。
  useTauriEvent('auth://state', (status) => {
    if (status.state === 'authenticated') {
      void finishLogin()
      return
    }
    if (status.state === 'error' || status.state === 'reauth_required') {
      setPhase('error')
      setMessage(status.message ?? '登录失败，请重新尝试。')
      setNotice(null)
      return
    }
    if (status.state === 'pending') {
      setPhase((current) => (current === 'waiting' ? current : 'waiting'))
      if (status.message && !status.message.startsWith('等待浏览器')) {
        setNotice(status.message)
      }
    }
  })

  // 深链到达（即使不含凭据）也立刻补一次轮询，缩短登录完成时延。
  useTauriEvent('auth://callback', () => {
    pollNowRef.current?.()
  })

  useEffect(() => {
    if (phase !== 'waiting') return

    let cancelled = false
    /** 本链最多一个轮询在飞：深链事件触发的立即轮询不能和定时轮询并发成两条链。 */
    let inFlight = false
    /** 在飞期间又被触发的“立即轮询”：等本次结束后补一次。 */
    let pending = false
    /** 本链已到终态（成功/失败/超时）：不再安排后续轮询。 */
    let stopped = false
    let timer: number | undefined

    const clearTimer = () => {
      if (timer !== undefined) {
        window.clearTimeout(timer)
        timer = undefined
      }
    }

    const schedule = (delayMs: number) => {
      if (cancelled || stopped) return
      // 先清旧句柄再设新的：旧实现直接覆盖 `timer`，会让上一条 timer 继续存活并再 poll 一次。
      clearTimer()
      timer = window.setTimeout(() => void poll(), delayMs)
    }

    const stop = () => {
      stopped = true
      clearTimer()
    }

    async function poll() {
      if (cancelled || stopped) return
      if (inFlight) {
        pending = true
        return
      }
      inFlight = true

      try {
        const started = startedAt.current
        const timeoutMs = (session?.timeout_secs ?? DEFAULT_TIMEOUT_MS / 1000) * 1000
        if (started != null && Date.now() - started > timeoutMs) {
          stop()
          setPhase('error')
          setMessage('登录超时，请重试；若浏览器或网络策略拦截了回调，可改用其它网络后重试。')
          return
        }

        // 后端命令返回完整 AuthStatus（不是简化的 LoginPoll）。
        const outcome = await runPollStatus()
        if (cancelled || stopped) return
        if (outcome.status === 'stale') return
        if (outcome.status === 'error') {
          // 单次命令/网络失败不判死，连续失败达到阈值才提示失败，仍可手动重试。
          pollFailures.current += 1
          if (pollFailures.current >= MAX_POLL_FAILURES) {
            stop()
            setPhase('error')
            setMessage(`轮询登录状态失败：${outcome.error.raw}`)
            return
          }
          setNotice(
            `轮询登录状态失败，正在重试（${pollFailures.current}/${MAX_POLL_FAILURES}）：${outcome.error.raw}`
          )
          schedule(DEFAULT_POLL_INTERVAL_MS)
          return
        }

        const status = outcome.data
        pollFailures.current = 0

        if (status.state === 'authenticated') {
          stop()
          await finishLogin()
          return
        }
        if (status.state === 'error' || status.state === 'reauth_required') {
          stop()
          setPhase('error')
          setMessage(status.message ?? '登录失败，请重新尝试。')
          return
        }
        if (status.state === 'unauthenticated') {
          stop()
          setPhase('error')
          setMessage('登录会话已结束，请重新发起登录。')
          return
        }

        if (status.message && !status.message.startsWith('等待浏览器')) {
          setNotice(status.message)
        }
        const intervalMs = (session?.poll_interval_secs ?? DEFAULT_POLL_INTERVAL_MS / 1000) * 1000
        schedule(intervalMs)
      } finally {
        inFlight = false
        // 在飞期间收到过“立即轮询”：本次结束后补一次，而不是并发再开一条链。
        if (!cancelled && !stopped && pending) {
          pending = false
          schedule(0)
        }
      }
    }

    pollNowRef.current = () => {
      if (cancelled || stopped) return
      if (inFlight) {
        pending = true
        return
      }
      // 深链到达：取消待触发的定时器，立即轮询一次。
      clearTimer()
      void poll()
    }
    schedule(DEFAULT_POLL_INTERVAL_MS)

    return () => {
      cancelled = true
      pollNowRef.current = null
      clearTimer()
    }
  }, [phase, session, finishLogin, runPollStatus])

  async function startLogin() {
    finished.current = false
    pollFailures.current = 0
    setPhase('starting')
    setMessage(null)
    setNotice(null)
    setSession(null)
    const outcome = await runStartLogin()
    if (outcome.status === 'stale') return
    if (outcome.status === 'error') {
      setPhase('error')
      setMessage(outcome.error.raw)
      return
    }
    const next = outcome.data
    if (!next.login_url) {
      setPhase('error')
      setMessage(
        '浏览器预览模式：没有检测到 Tauri 后端，无法唤起系统浏览器登录。可点击下方“跳过登录，预览界面”查看界面。'
      )
      return
    }
    startedAt.current = Date.now()
    setSession(next)
    setPhase('waiting')
    setNotice(
      next.opened_browser
        ? next.hint
        : '未能自动打开系统浏览器，请复制下方登录链接到浏览器中打开。'
    )
  }

  function cancelWaiting() {
    startedAt.current = null
    setSession(null)
    setNotice(null)
    setMessage(null)
    setPhase('idle')
  }

  async function submitManualCallback() {
    const url = manualUrl.trim()
    if (!url) {
      setNotice('请先粘贴浏览器回调链接（以 codemakerhub-light:// 开头）。')
      return
    }
    setManualBusy(true)
    setMessage(null)
    try {
      const outcome = await runSubmitDeepLink({ url })
      if (outcome.status === 'stale') return
      if (outcome.status === 'error') {
        setPhase('error')
        setMessage(outcome.error.raw)
        return
      }
      const status = outcome.data
      if (status.state === 'authenticated') {
        await finishLogin()
        return
      }
      setPhase('error')
      setMessage(status.message ?? '回调链接已提交，但登录未完成，请重新登录。')
    } finally {
      setManualBusy(false)
      // 链接内含一次性凭据，提交后立即从前端状态清除。
      setManualUrl('')
    }
  }

  async function copyLoginUrl() {
    if (!session?.login_url) return
    try {
      await navigator.clipboard.writeText(session.login_url)
      setCopyState('copied')
      window.setTimeout(() => setCopyState('idle'), 1500)
    } catch {
      setCopyState('failed')
      window.setTimeout(() => setCopyState('idle'), 2000)
    }
  }

  const busy = phase === 'starting' || phase === 'waiting'
  const title =
    phase === 'waiting' ? '等待浏览器完成登录' : phase === 'error' ? '登录未完成' : '登录以继续'

  return (
    <div className="login-screen">
      <div className="login-glow" aria-hidden="true" />
      <section className="login-card">
        <div className="login-brand">
          <span className="brand-mark brand-mark--lg">CH</span>
          <div>
            <p className="login-brand-name">CodeMaker Hub Light</p>
            <p className="login-brand-sub">账号登录 · 本地代理 · 用量统计</p>
          </div>
        </div>

        <h1 className="login-title">{title}</h1>
        <p className="login-desc">
          在系统浏览器中完成登录。token 只保存在本机 Rust 侧（加密文件 + 系统凭据库），
          前端、日志与数据库都不会出现凭据明文。
        </p>

        <div className="login-status" role="status" aria-live="polite">
          {phase === 'error' && message ? (
            <p className="inline-message inline-message--error">{message}</p>
          ) : phase === 'waiting' ? (
            <p className="login-waiting">
              <span className="spinner" aria-hidden="true" />
              请在弹出的浏览器页面完成登录，本页会自动更新…
            </p>
          ) : (
            <p className="login-hint">
              需要能访问你在「设置 → 代理与记录」中配置的网关地址。
            </p>
          )}

          {phase === 'waiting' && notice ? (
            <p className="login-notice" role="status">
              {notice}
            </p>
          ) : null}
        </div>

        {session && !session.opened_browser ? (
          <div className="login-url">
            <p>登录链接（仅含一次性 pollUuid 与回调地址，不含 token）：</p>
            <div className="login-url-row">
              <code className="login-url-text">{session.login_url}</code>
              <button type="button" className="btn btn--ghost btn--sm" onClick={() => void copyLoginUrl()}>
                {copyState === 'copied' ? '已复制' : copyState === 'failed' ? '复制失败' : '复制'}
              </button>
            </div>
          </div>
        ) : null}

        <div className="login-actions">
          <button
            type="button"
            className="btn btn--primary btn--block"
            onClick={() => void startLogin()}
            disabled={busy}
          >
            {phase === 'starting'
              ? '正在唤起浏览器…'
              : phase === 'error'
                ? '重新登录'
                : phase === 'waiting'
                  ? '等待中…'
                  : '登录'}
          </button>
          {phase === 'waiting' ? (
            <button type="button" className="btn btn--ghost btn--block" onClick={cancelWaiting}>
              取消等待
            </button>
          ) : null}
        </div>

        <details className="login-manual">
          <summary>浏览器没有自动跳回应用？手动粘贴回调链接</summary>
          <p>
            完成登录后，浏览器会跳到 <code>codemakerhub-light://auth/callback?...</code>。
            若应用未自动打开，请复制地址栏中的完整链接粘贴到下方。链接含一次性凭据，请勿截图或分享；
            提交后前端会立即清除输入框，解析与保存只在 Rust 侧完成。
          </p>
          <div className="login-url-row">
            <input
              className="login-url-input"
              type="text"
              value={manualUrl}
              onChange={(event) => setManualUrl(event.target.value)}
              placeholder="codemakerhub-light://auth/callback?pollUuid=...?access_token=..."
              spellCheck={false}
            />
            <button
              type="button"
              className="btn btn--ghost btn--sm"
              onClick={() => void submitManualCallback()}
              disabled={manualBusy}
            >
              {manualBusy ? '提交中…' : '提交'}
            </button>
          </div>
        </details>

        {previewMode ? (
          <div className="login-preview">
            <p>当前是浏览器预览：没有 Tauri 后端，登录 / 代理 / 数据库均为 mock。</p>
            <button type="button" className="btn btn--ghost btn--sm" onClick={onPreviewSignIn}>
              跳过登录，预览界面
            </button>
          </div>
        ) : null}

        <ul className="login-points">
          <li>代理只监听 127.0.0.1，不对外网暴露端口</li>
          <li>请求正文默认不记录，可在设置中显式开启</li>
          <li>退出登录会清除本地加密凭据，代理立即返回 401</li>
        </ul>
      </section>
    </div>
  )
}
