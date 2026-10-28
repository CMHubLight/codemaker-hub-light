interface EmptyStateProps {
  title: string
  text: string
}

/** 面板内空态/错误占位：概览页多个面板共用的排版。 */
export function EmptyState({ title, text }: EmptyStateProps) {
  return (
    <div className="empty-state">
      <p className="empty-state-title">{title}</p>
      <p className="empty-state-text">{text}</p>
    </div>
  )
}
