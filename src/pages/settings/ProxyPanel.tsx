import { Panel, PanelHeader } from '../../components/Panel'
import { StatusPill } from '../../components/StatusPill'
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
  /** 当前表单里的有效端口（未保存的编辑也会立即反映在展示地址上）。 */
  configuredPort: number
  configuredBind: string
  configuredGateway: string | null
  onRestart: () => void
  restarting: boolean
  loading: boolean
}

/** 本地代理状态面板：运行态、实际监听端口、端口回退提示与重启入口。 */
export function ProxyPanel({
  proxy,
  error,
  restartMessage,
  savedPort,
  configuredPort,
  configuredBind,
  configuredGateway,
  onRestart,
  restarting,
  loading
}: ProxyPanelProps) {
  const baseUrl = proxy?.running ? proxy.base_url : `http://127.0.0.1:${configuredPort}`
  // 注意：后端在“未运行”时返回的是默认端口（15721），回退提示只应在运行态比较。
  const fallbackActive = proxy?.running === true && savedPort != null && proxy.port !== savedPort
  const listenAddress = proxy?.running
    ? `${proxy.host}:${proxy.port}`
    : `${configuredBind}:${configuredPort}`
  const gatewayValue = proxy?.running
    ? proxy.gateway_url
    : (configuredGateway ?? proxy?.gateway_url ?? '—')
  const healthUrl = proxy?.running ? proxy.health_url : `${baseUrl}/health`

  return (
    <Panel>
      <PanelHeader title="本地代理" meta="仅监听回环地址，拒绝浏览器页面直接调用">
        <div className="panel-actions">
          <StatusPill
            status={
              proxy == null ? 'idle' : proxy.running ? 'ok' : proxy.last_error ? 'error' : 'warn'
            }
            label={
              proxy == null
                ? '读取中'
                : proxy.running
                  ? '运行中'
                  : proxy.last_error
                    ? '启动失败'
                    : '未运行'
            }
          />
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
      {error ? (
        <p className="inline-message inline-message--error" role="alert">
          读取代理状态失败：{error}
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
