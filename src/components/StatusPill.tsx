export type PillStatus = 'ok' | 'warn' | 'error' | 'info' | 'idle'

interface StatusPillProps {
  status: PillStatus
  label: string
}

/** 状态胶囊：登录态、代理状态、保存状态共用同一套视觉语义。 */
export function StatusPill({ status, label }: StatusPillProps) {
  return (
    <span className={`pill pill--${status}`}>
      <span className="pill-dot" aria-hidden="true" />
      {label}
    </span>
  )
}
