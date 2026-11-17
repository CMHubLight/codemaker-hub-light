import { Panel, PanelHeader } from '../../components/Panel'
import { StatusPill } from '../../components/StatusPill'
import type { PillStatus } from '../../components/StatusPill'
import { formatDateTime } from '../../lib/format'
import { formatUptime } from '../../lib/labels'
import type { ProxyStatus } from '../../lib/types'
import type { SaveState } from './types'

interface ProxyPanelProps {
  proxy: ProxyStatus | null
  error: string | null
  restartMessage: SaveState | null
  /** 已保存的配置端口：只和它比较才能判断后端是否发生端口回退。 */
  savedPort: number | null
  /** 已保存新端口/网关但代理尚未重启：此时的端口差异是预期行为，不是回退。 */
  restartPending: boolean
  /** 当前表单里的有效端口（未保存的编辑也会立即反映在展示地址上）。 */
  configuredPort: number
  configuredBind: string
  configuredGateway: string | null
  onRestart: () => void
  /** 读取代理状态失败时重试。 */
  onRetry: () => void
  restarting: boolean
  /** 代理状态本身是否正在读取（与设置表单的加载态分开）。 */
  loading: boolean
}

/** 本地代理状态面板：运行态、实际监听端口、端口回退提示与重启入口。 */
export function ProxyPanel({
  proxy,
  error,
  restartMessage,
  savedPort,
  restartPending,
  configuredPort,
  configuredBind,
  configuredGateway,
  onRestart,
  onRetry,
  restarting,
  loading
}: ProxyPanelProps) {
  const baseUrl = proxy?.running ? proxy.base_url : `http://127.0.0.1:${configuredPort}`
  // 注意：后端在“未运行”时返回的是默认端口（15721），回退提示只应在运行态比较；
  // 且保存了未重启的新端口时（restartPending）差异是应用要求的中间态（N-P2-3）。
  const fallbackActive =
    proxy?.running === true && !restartPending && savedPort != null && proxy.port !== savedPort
  const listenAddress = proxy?.running
    ? `${proxy.host}:${proxy.port}`
    : `${configuredBind}:${configuredPort}`
  const gatewayValue = proxy?.running
    ? proxy.gateway_url
    : (configuredGateway ?? proxy?.gateway_url ?? '—')
  const healthUrl = proxy?.running ? proxy.health_url : `${baseUrl}/health`
  // `proxy == null` 既可能是“首次读取中”也可能是“读取失败”：用 loading/error 分开判，
  // 失败时给明确状态与重试入口，而不是永远显示“读取中”（旧 P2-5）。
  const status: { status: PillStatus; label: string } =
    proxy == null
      ? error
        ? { status: 'error', label: '读取失败' }
        : loading
          ? { status: 'idle', label: '读取中' }
          : { status: 'idle', label: '未知' }
      : proxy.running
        ? { status: 'ok', label: '运行中' }
        : proxy.last_error
          ? { status: 'error', label: '启动失败' }
          : { status: 'warn', label: '未运行' }

  return (
    <Panel>
      <PanelHeader title="本地代理" meta="仅监听回环地址，拒绝浏览器页面直接调用">
        <div className="panel-actions">
          <StatusPill status={status.status} label={status.label} />
          <button
            type="button"
            className="btn btn--ghost btn--sm"
            onClick={onRestart}
            disabled={restarting || loading}
          >
            {restarting ? '重启中…' : '重启代理'}
          </button>
        </div>
      </PanelHeader>

      {error ? (
        <div className="inline-message-row">
          <p className="inline-message inline-message--error" role="alert">
            读取代理状态失败：{error}
          </p>
          <button type="button" className="btn btn--ghost btn--sm" onClick={onRetry}>
            重试
          </button>
        </div>
      ) : null}

      <dl className="kv">
        <div>
          <dt>监听地址</dt>
          <dd className="mono">{listenAddress}</dd>
        </div>
        <div>
          <dt>客户端 Base URL</dt>
          <dd className="mono">{baseUrl}</dd>
        </div>
        <div>
          <dt>上游网关</dt>
          <dd className="mono">{gatewayValue}</dd>
        </div>
        <div>
          <dt>健康检查</dt>
          <dd className="mono">{healthUrl}</dd>
        </div>
        <div>
          <dt>运行时长</dt>
          <dd>
            {proxy?.running ? formatUptime(proxy.uptime_secs) : '未运行'}
            {proxy?.running && proxy.started_at
              ? ` · 启动于 ${formatDateTime(proxy.started_at)}`
              : ''}
          </dd>
        </div>
        <div>
          <dt>请求 / 错误</dt>
          <dd>
            {proxy ? `${proxy.request_count} / ${proxy.error_count}` : '—'}
            {proxy?.running ? '' : '（启动后计数）'}
          </dd>
        </div>
        <div>
          <dt>端口文件</dt>
          <dd className="mono">{proxy?.port_file || '—'}</dd>
        </div>
      </dl>

      {fallbackActive ? (
        <p className="inline-message inline-message--warn" role="status">
          端口回退：配置端口 {savedPort} 不可用，代理实际监听 {proxy.port}。
        </p>
      ) : null}
      {proxy?.last_error ? (
        <p className="inline-message inline-message--error" role="alert">
          代理错误：{proxy.last_error}
        </p>
      ) : null}
      {restartMessage ? (
        <p className={`inline-message inline-message--${restartMessage.tone}`} role="status">
          {restartMessage.text}
        </p>
      ) : null}

      <p className="hint">
        代理随应用启动；端口被占用时自动回退到 15722~15800，实际端口写入 proxy.port。
        修改端口或网关后需先保存，再重启代理；记录正文与积分换算保存后立即生效。
      </p>
    </Panel>
  )
}
