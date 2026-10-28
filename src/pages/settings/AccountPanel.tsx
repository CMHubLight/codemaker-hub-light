import { Panel, PanelHeader } from '../../components/Panel'
import { StatusPill } from '../../components/StatusPill'
import { formatDateTime } from '../../lib/format'
import type { AuthStatus } from '../../lib/types'

interface AccountPanelProps {
  auth: AuthStatus | null
  loggingOut: boolean
  logoutError: string | null
  onLogout: () => void
}

/** 账户面板：登录账号诊断信息 + 退出登录。 */
export function AccountPanel({ auth, loggingOut, logoutError, onLogout }: AccountPanelProps) {
  // 只有 authenticated 才有完整的账号字段可展示（pending/error 时 auth 可能是空壳）。
  const account = auth?.state === 'authenticated' ? auth : null

  return (
    <Panel>
      <PanelHeader
        title="账户"
        meta="登录凭据保存在本机加密文件 + 系统凭据库，前端不接触 token"
      >
        {account ? (
          <StatusPill
            status={account.offline_grace ? 'warn' : 'ok'}
            label={account.offline_grace ? '离线宽限' : '已登录'}
          />
        ) : (
          <StatusPill status="idle" label="未登录" />
        )}
      </PanelHeader>

      {account ? (
        <dl className="kv">
          <div>
            <dt>账号</dt>
            <dd>{account.display_name ?? account.user ?? '—'}</dd>
          </div>
          <div>
            <dt>部门</dt>
            <dd>{account.dept ?? '—'}</dd>
          </div>
          <div>
            <dt>登录网关</dt>
            <dd className="mono">{account.server_url ?? '—'}</dd>
          </div>
          <div>
            <dt>签发时间</dt>
            <dd>{formatDateTime(account.issued_at)}</dd>
          </div>
          <div>
            <dt>有效期至</dt>
            <dd>{formatDateTime(account.expires_at)}</dd>
          </div>
          <div>
            <dt>最近校验</dt>
            <dd>{formatDateTime(account.last_validated_at)}</dd>
          </div>
          <div>
            <dt>凭据状态</dt>
            <dd>{account.credentials_valid ? '有效' : '待校验 / 已失效'}</dd>
          </div>
        </dl>
      ) : (
        <p className="hint">当前没有可展示的账号信息，请重新登录。</p>
      )}

      <div className="form-actions">
        <button type="button" className="btn btn--danger" onClick={onLogout} disabled={loggingOut}>
          {loggingOut ? '正在退出…' : '退出登录'}
        </button>
        <span className="hint">
          退出后清除本地加密凭据，代理立即返回 401，需要重新登录才能继续使用。
        </span>
      </div>

      {logoutError ? (
        <p className="inline-message inline-message--error" role="alert">
          {logoutError}
        </p>
      ) : null}
    </Panel>
  )
}
