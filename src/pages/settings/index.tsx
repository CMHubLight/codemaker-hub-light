import { useEffect, useState } from 'react'
import type { FormEvent } from 'react'
import { useAsyncResource, useCommand } from '../../lib/async'
import { invokeCmd, isTauri } from '../../lib/ipc'
import {
  applyNumberDrafts,
  isSettingsDirty,
  numberDraftError,
  numberDraftsFrom,
  parseNumberDraft,
  settingsPatch,
  validateSettings
} from '../../lib/settingsForm'
import type { NumberDrafts, NumberField } from '../../lib/settingsForm'
import type { AppInfo, AppLogResult, AppSettings, AuthStatus, ProxyStatus } from '../../lib/types'
import { AccountPanel } from './AccountPanel'
import { AppInfoPanel } from './AppInfoPanel'
import { IntegrationGuide } from './IntegrationGuide'
import { ProxyPanel } from './ProxyPanel'
import { RecordingForm } from './RecordingForm'
import type { SaveState } from './types'

interface SettingsProps {
  auth: AuthStatus | null
  /** App 在本机数据变化后递增：刷新代理状态与应用信息，不覆盖未保存的设置编辑。 */
  revision?: number
  onLogout: () => Promise<void> | void
  /** 手动续期成功后的登录态更新（后端同时会发 auth://state，这里用于即时刷新面板）。 */
  onAuthUpdated?: (status: AuthStatus) => void
}

/**
 * 设置页：真实代理状态（端口/回退/运行时长）、设置读写、账户、接入说明与应用信息。
 * 设置保存走 `settings_update`；代理启停/端口回退由 Rust 侧管理。
 *
 * 取数走 `useAsyncResource`：设置表单只在首次进入和显式重试时读取（事件刷新不覆盖
 * 未保存编辑），代理状态与应用信息按 `revision` 静默刷新。
 * 数字字段用编辑态字符串（`NumberDrafts`），失焦/提交才解析校验，清空不会立刻变 0。
 */
export function Settings({ auth, revision = 0, onLogout, onAuthUpdated }: SettingsProps) {
  const previewMode = !isTauri()
  const [form, setForm] = useState<AppSettings | null>(null)
  const [drafts, setDrafts] = useState<NumberDrafts | null>(null)
  const [savedForm, setSavedForm] = useState<AppSettings | null>(null)
  const [saveState, setSaveState] = useState<SaveState | null>(null)
  const [restartMessage, setRestartMessage] = useState<SaveState | null>(null)
  const [logsMessage, setLogsMessage] = useState<SaveState | null>(null)
  const [loggingOut, setLoggingOut] = useState(false)
  const [logoutError, setLogoutError] = useState<string | null>(null)
  const [renewalMessage, setRenewalMessage] = useState<SaveState | null>(null)

  // 设置只在首次进入和显式重试时读取：key 不含 revision，避免事件刷新覆盖用户编辑。
  const settingsResource = useAsyncResource('settings', () => invokeCmd<AppSettings>('settings_get'))
  const proxyResource = useAsyncResource(`proxy:${revision}`, () =>
    invokeCmd<ProxyStatus>('proxy_status')
  )
  const appInfoResource = useAsyncResource(`app_info:${revision}`, () =>
    invokeCmd<AppInfo>('app_info')
  )

  const { run: runSave, running: saving } = useCommand<AppSettings>('settings_update')
  const { run: runRestart, running: restarting } = useCommand<ProxyStatus>('proxy_restart')
  const { run: runOpenLogs, running: openingLogs } = useCommand<AppLogResult>('app_open_logs')
  const { run: runRetryRenewal, running: retryingRenewal } = useCommand<AuthStatus>(
    'auth_retry_renewal'
  )

  useEffect(() => {
    const next = settingsResource.data
    if (!next) return
    setForm(next)
    setDrafts(numberDraftsFrom(next))
    setSavedForm(next)
  }, [settingsResource.data])

  const loading = settingsResource.loading || settingsResource.refreshing
  const loadError = settingsResource.error?.raw ?? null
  const proxy = proxyResource.data
  const proxyError = proxyResource.error?.raw ?? null
  const appInfo = appInfoResource.data
  const infoError = appInfoResource.error?.raw ?? null

  const dirty =
    form != null && drafts != null && savedForm != null
      ? isSettingsDirty({ settings: form, drafts }, savedForm)
      : false
  // 展示用端口取“当前有效编辑值”：输入合法数字时实时跟随，非法/空串时退回上次有效值。
  const configuredPort = drafts
    ? (parseNumberDraft('proxy_port', drafts.proxy_port) ?? form?.proxy_port ?? 15721)
    : (form?.proxy_port ?? 15721)
  const savedPort = savedForm?.proxy_port ?? null
  const baseUrl = proxy?.running ? proxy.base_url : `http://127.0.0.1:${configuredPort}`

  function changeGateway(value: string) {
    setForm((current) => (current ? { ...current, gateway_url: value } : current))
  }

  function toggleRecordBody(value: boolean) {
    setForm((current) => (current ? { ...current, record_body: value } : current))
  }

  function changeNumber(field: NumberField, raw: string) {
    setDrafts((current) => (current ? { ...current, [field]: raw } : current))
  }

  function commitNumber(field: NumberField) {
    if (!form || !drafts) return
    const error = numberDraftError(field, drafts[field])
    if (error) {
      setSaveState({ tone: 'error', text: error })
      return
    }
    const parsed = parseNumberDraft(field, drafts[field])
    if (parsed == null) return
    if (parsed !== form[field]) setForm({ ...form, [field]: parsed })
  }

  function resetForm() {
    if (!savedForm) return
    setForm(savedForm)
    setDrafts(numberDraftsFrom(savedForm))
  }

  async function handleSave(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    if (!form || !drafts) return

    // 数字字段先给具体提示（空串/越界），再走完整校验（网关 URL 等）。
    const numberError =
      numberDraftError('proxy_port', drafts.proxy_port) ??
      numberDraftError('retention_days', drafts.retention_days)
    if (numberError) {
      setSaveState({ tone: 'error', text: numberError })
      return
    }
    const candidate = applyNumberDrafts(form, drafts)
    if (!candidate) {
      setSaveState({ tone: 'error', text: '数字设置不合法，请检查后重试。' })
      return
    }
    const validation = validateSettings(candidate)
    if (validation) {
      setSaveState({ tone: 'error', text: validation })
      return
    }

    setSaveState(null)
    const outcome = await runSave({ settings: settingsPatch(candidate) })
    if (outcome.status === 'error') {
      setSaveState({ tone: 'error', text: outcome.error.raw })
      return
    }
    if (outcome.status === 'stale') return

    const saved = outcome.data
    const needsRestart =
      saved.proxy_port !== savedForm?.proxy_port || saved.gateway_url !== savedForm?.gateway_url
    const liveOnly =
      !needsRestart &&
      (saved.record_body !== savedForm?.record_body ||
        saved.credit_to_yuan !== savedForm?.credit_to_yuan)
    setForm(saved)
    setDrafts(numberDraftsFrom(saved))
    setSavedForm(saved)
    settingsResource.mutate(saved)
    setSaveState({
      tone: 'ok',
      text: previewMode
        ? '已保存（浏览器预览：仅本次会话内存生效）。'
        : needsRestart
          ? '设置已保存。端口或网关已变更，请重启代理后生效。'
          : liveOnly
            ? '设置已保存，并在下一次请求立即生效（无需重启代理）。'
            : '设置已保存。'
    })
    if (needsRestart) proxyResource.reload()
  }

  async function handleRestartProxy() {
    setRestartMessage(null)
    const outcome = await runRestart()
    if (outcome.status === 'ok') {
      const next = outcome.data
      proxyResource.mutate(next)
      // 回退提示只和“已保存的配置端口”比较：未保存的编辑不应影响这里的判断。
      const fallback =
        savedPort != null && next.port !== savedPort
          ? `（配置端口 ${savedPort} 被占用，已回退到 ${next.port}）`
          : ''
      setRestartMessage({ tone: 'ok', text: `代理已重启：${next.base_url}${fallback}` })
    } else if (outcome.status === 'error') {
      setRestartMessage({ tone: 'error', text: outcome.error.raw })
    }
  }

  async function handleOpenLogs() {
    setLogsMessage(null)
    const outcome = await runOpenLogs()
    if (outcome.status === 'ok') {
      const result = outcome.data
      setLogsMessage({
        tone: result.opened ? 'ok' : 'error',
        text: result.opened ? '已打开日志目录。' : (result.message ?? '打开日志目录失败。')
      })
    } else if (outcome.status === 'error') {
      setLogsMessage({ tone: 'error', text: outcome.error.raw })
    }
  }

  async function handleLogout() {
    setLoggingOut(true)
    setLogoutError(null)
    try {
      await onLogout()
    } catch (cause) {
      setLogoutError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setLoggingOut(false)
    }
  }

  async function handleRetryRenewal() {
    setRenewalMessage(null)
    const outcome = await runRetryRenewal()
    if (outcome.status === 'error') {
      setRenewalMessage({ tone: 'error', text: `重新检测失败：${outcome.error.raw}` })
      return
    }
    if (outcome.status === 'stale') return
    const status = outcome.data
    onAuthUpdated?.(status)
    if (status.state === 'reauth_required') {
      setRenewalMessage({
        tone: 'error',
        text: status.message ?? '重新检测后仍未恢复登录，请重新登录。'
      })
    } else if (status.state === 'authenticated' && !status.message) {
      setRenewalMessage({ tone: 'ok', text: '已重新检测并完成续期，登录状态已刷新。' })
    } else {
      // 续期失败但保留登录态（网络失败/离线宽限）或其它异常：如实展示后端文案。
      setRenewalMessage({
        tone: 'error',
        text: status.message ?? '重新检测未完成，请稍后重试或重新登录。'
      })
    }
  }

  return (
    <div className="stack">
      {loadError ? (
        <div className="inline-message-row">
          <p className="inline-message inline-message--error" role="alert">
            读取设置失败：{loadError}
          </p>
          <button
            type="button"
            className="btn btn--ghost btn--sm"
            onClick={() => settingsResource.reload()}
          >
            重试
          </button>
        </div>
      ) : null}

      <ProxyPanel
        proxy={proxy}
        error={proxyError}
        restartMessage={restartMessage}
        savedPort={savedPort}
        configuredPort={configuredPort}
        configuredBind={form?.proxy_bind ?? '127.0.0.1'}
        configuredGateway={form?.gateway_url ?? null}
        onRestart={() => void handleRestartProxy()}
        restarting={restarting}
        loading={loading}
      />

      <RecordingForm
        form={form}
        drafts={drafts}
        loading={loading}
        saving={saving}
        dirty={dirty}
        saveState={saveState}
        onSave={(event) => void handleSave(event)}
        onChangeGateway={changeGateway}
        onToggleRecordBody={toggleRecordBody}
        onChangeNumber={changeNumber}
        onCommitNumber={commitNumber}
        onReset={resetForm}
      />

      <AccountPanel
        auth={auth}
        loggingOut={loggingOut}
        logoutError={logoutError}
        retryingRenewal={retryingRenewal}
        renewalMessage={renewalMessage}
        onRetryRenewal={() => void handleRetryRenewal()}
        onLogout={() => void handleLogout()}
      />

      <IntegrationGuide baseUrl={baseUrl} />

      <AppInfoPanel
        appInfo={appInfo}
        infoError={infoError}
        previewMode={previewMode}
        openingLogs={openingLogs}
        logsMessage={logsMessage}
        onOpenLogs={() => void handleOpenLogs()}
      />
    </div>
  )
}
