import type { FormEventHandler, ReactNode } from 'react'

interface PanelProps {
  children: ReactNode
  /** 面板默认 section；设置页的“代理与记录”需要 form 语义（回车提交 / submit 按钮）。 */
  as?: 'section' | 'form'
  onSubmit?: FormEventHandler<HTMLFormElement>
  /** 额外 class（例如错误边界的 `error-boundary`）。 */
  className?: string
  /** 语义角色（错误边界用 alert；普通面板不需要）。 */
  role?: string
}

/**
 * 面板外壳原语：统一 `section.panel` / `form.panel` 的标签与 class，
 * 避免每个页面重复 `<section className="panel">…`。视觉结构与原实现完全一致。
 */
export function Panel({ children, as = 'section', onSubmit, className, role }: PanelProps) {
  const classes = className ? `panel ${className}` : 'panel'
  if (as === 'form') {
    return (
      <form className={classes} onSubmit={onSubmit} role={role}>
        {children}
      </form>
    )
  }
  return (
    <section className={classes} role={role}>
      {children}
    </section>
  )
}

interface PanelHeaderProps {
  title: string
  /** 标题下的说明；省略时只渲染 h2（与原来手写 header 的结构一致）。 */
  meta?: ReactNode
  /** 右侧操作区：状态胶囊、按钮组、panel-tag 或分段控件。 */
  children?: ReactNode
}

/** 面板标题原语：左侧标题 + 说明，右侧由调用方决定。 */
export function PanelHeader({ title, meta, children }: PanelHeaderProps) {
  return (
    <header className="panel-header">
      <div>
        <h2>{title}</h2>
        {meta == null ? null : <p className="panel-meta">{meta}</p>}
      </div>
      {children}
    </header>
  )
}
