import type { FormEvent } from 'react'
import { Panel, PanelHeader } from '../../components/Panel'
import { StatusPill } from '../../components/StatusPill'
import type { NumberDrafts, NumberField } from '../../lib/settingsForm'
import type { AppSettings } from '../../lib/types'
import type { SaveState } from './types'

interface RecordingFormProps {
  form: AppSettings | null
  drafts: NumberDrafts | null
  loading: boolean
  saving: boolean
  dirty: boolean
  saveState: SaveState | null
  onSave: (event: FormEvent<HTMLFormElement>) => void
  onChangeGateway: (value: string) => void
  onToggleRecordBody: (value: boolean) => void
  /** 数字输入只改编辑态字符串，不立即解析成 0（可清空、失焦才校验）。 */
  onChangeNumber: (field: NumberField, raw: string) => void
  onCommitNumber: (field: NumberField) => void
  onReset: () => void
}

/** 代理与记录设置表单：端口/网关/保留天数/正文记录开关。 */
export function RecordingForm({
  form,
  drafts,
  loading,
  saving,
  dirty,
  saveState,
  onSave,
  onChangeGateway,
  onToggleRecordBody,
  onChangeNumber,
  onCommitNumber,
  onReset
}: RecordingFormProps) {
  return (
    <Panel as="form" onSubmit={onSave}>
      <PanelHeader title="代理与记录" meta="保存后写入本地设置文件；端口/网关变更需重启代理">
        {dirty ? (
          <StatusPill status="warn" label="有未保存修改" />
        ) : (
          <StatusPill status="ok" label="已同步" />
        )}
      </PanelHeader>

      {form && drafts ? (
        <div className="form-grid">
          <label className="field">
            <span>代理端口</span>
            <input
              type="number"
              min={1024}
              max={65535}
              value={drafts.proxy_port}
              onChange={(event) => onChangeNumber('proxy_port', event.target.value)}
              onBlur={() => onCommitNumber('proxy_port')}
              disabled={loading || saving}
            />
            <small>默认 15721；被占用时由后端自动回退。</small>
          </label>

          <label className="field">
            <span>网关地址</span>
            <input
              type="text"
              value={form.gateway_url}
              onChange={(event) => onChangeGateway(event.target.value)}
              disabled={loading || saving}
              spellCheck={false}
            />
            <small>上游 API 网关，用于登录校验与请求转发；默认值是占位地址，请填写可访问的兼容网关。</small>
          </label>

          <label className="field">
            <span>用量保留天数</span>
            <input
              type="number"
              min={1}
              max={3650}
              value={drafts.retention_days}
              onChange={(event) => onChangeNumber('retention_days', event.target.value)}
              onBlur={() => onCommitNumber('retention_days')}
              disabled={loading || saving}
            />
            <small>默认 90 天；启动后与每 24 小时按该天数清理本机请求明细（不会删除 exports 里的 CSV）。</small>
          </label>

          <label className="field field--switch">
            <span>记录请求正文</span>
            <span className="switch">
              <input
                type="checkbox"
                checked={form.record_body}
                onChange={(event) => onToggleRecordBody(event.target.checked)}
                disabled={loading || saving}
              />
              <span className="switch-track" aria-hidden="true" />
            </span>
            <small>
              默认关闭。开启后请求/响应正文按 64KB 截断写入本地 SQLite，可能包含代码等敏感内容。
            </small>
          </label>
        </div>
      ) : (
        <p className="trend-note">{loading ? '正在读取设置…' : '设置不可用，请先重试读取。'}</p>
      )}

      <div className="form-actions">
        <button type="submit" className="btn btn--primary" disabled={!form || loading || saving}>
          {saving ? '保存中…' : '保存设置'}
        </button>
        <button
          type="button"
          className="btn btn--ghost"
          onClick={onReset}
          disabled={!dirty || saving}
        >
          撤销修改
        </button>
        {saveState ? (
          <span className={`inline-message inline-message--${saveState.tone}`} role="status">
            {saveState.text}
          </span>
        ) : null}
      </div>
    </Panel>
  )
}
