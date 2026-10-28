import { Panel, PanelHeader } from '../../components/Panel'
import type { AppInfo } from '../../lib/types'
import type { SaveState } from './types'

interface AppInfoPanelProps {
  appInfo: AppInfo | null
  infoError: string | null
  previewMode: boolean
  openingLogs: boolean
  logsMessage: SaveState | null
  onOpenLogs: () => void
}

/** 应用信息面板：版本、数据目录与诊断路径，附带“打开日志目录”。 */
export function AppInfoPanel({
  appInfo,
  infoError,
  previewMode,
  openingLogs,
  logsMessage,
  onOpenLogs
}: AppInfoPanelProps) {
  return (
    <Panel>
      <PanelHeader title="应用信息" meta="版本、数据目录与诊断路径">
        <div className="panel-actions">
          <button
            type="button"
            className="btn btn--ghost btn--sm"
            onClick={onOpenLogs}
            disabled={openingLogs}
          >
            {openingLogs ? '打开中…' : '打开日志目录'}
          </button>
        </div>
      </PanelHeader>

      {appInfo ? (
        <dl className="kv">
          <div>
            <dt>版本</dt>
            <dd>
              {appInfo.name} v{appInfo.version}
              {appInfo.tauri_version ? ` · Tauri ${appInfo.tauri_version}` : ''}
            </dd>
          </div>
          <div>
            <dt>标识</dt>
            <dd className="mono">{appInfo.identifier}</dd>
          </div>
          <div>
            <dt>平台</dt>
            <dd>
              {appInfo.platform} / {appInfo.arch}
              {previewMode ? '（浏览器预览）' : ''}
            </dd>
          </div>
          <div>
            <dt>数据目录</dt>
            <dd className="mono">{appInfo.data_dir ?? '—'}</dd>
          </div>
          <div>
            <dt>数据库</dt>
            <dd className="mono">{appInfo.db_path || '—'}</dd>
          </div>
          <div>
            <dt>Schema 版本</dt>
            <dd>{appInfo.schema_version}</dd>
          </div>
          <div>
            <dt>日志目录</dt>
            <dd className="mono">{appInfo.logs_dir || '—'}</dd>
          </div>
          <div>
            <dt>代理端口文件</dt>
            <dd className="mono">{appInfo.proxy_port_file || '—'}</dd>
          </div>
        </dl>
      ) : infoError ? (
        <p className="inline-message inline-message--error" role="alert">
          读取应用信息失败：{infoError}
        </p>
      ) : (
        <p className="trend-note">正在读取应用信息…</p>
      )}

      {logsMessage ? (
        <p className={`inline-message inline-message--${logsMessage.tone}`} role="status">
          {logsMessage.text}
        </p>
      ) : null}
    </Panel>
  )
}
