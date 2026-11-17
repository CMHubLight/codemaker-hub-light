import type { ReactNode } from 'react'

interface EmptyStateProps {
  title: string
  text: string
  /** 占位卡内的操作入口（重试、回到第一页等）；没有操作时省略。 */
  action?: ReactNode
}

/** 面板内空态/错误占位：概览页多个面板共用的排版。 */
export function EmptyState({ title, text, action }: EmptyStateProps) {
  return (
    <div className="empty-state">
      <p className="empty-state-title">{title}</p>
      <p className="empty-state-text">{text}</p>
      {action ? <div className="empty-state-actions">{action}</div> : null}
    </div>
  )
}
