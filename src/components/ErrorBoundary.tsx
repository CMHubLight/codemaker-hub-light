import { Component } from 'react'
import type { ErrorInfo, ReactNode } from 'react'
import { errorMessage, invokeCmd } from '../lib/ipc'
import { Panel, PanelHeader } from './Panel'

interface ErrorBoundaryProps {
  children: ReactNode
  /** 出错范围（展示用，例如「概览」「应用」）。 */
  scope: string
  /** 点「重试」时的额外重置动作（例如回到默认页）。 */
  onReset?: () => void
  /** 根级边界用：重试时整页刷新（此时应用状态可能已不一致）。 */
  reloadOnReset?: boolean
}

interface ErrorBoundaryState {
  error: Error | null
}

/**
 * 渲染期错误兜底：React 的 error boundary 只能是 class 组件。
 *
 * 只捕获**渲染/生命周期**异常；effect 里的异步异常仍由各页面自己的 try/catch 处理
 * （`describeError` 负责分类）。根级边界避免整窗白屏，页面级边界让切页即可恢复。
 */
export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  override state: ErrorBoundaryState = { error: null }

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error }
  }

  override componentDidCatch(error: Error, info: ErrorInfo): void {
    // release 没有控制台，这里只做开发期提示；用户看到的是下面的中文卡片。
    console.error(`[${this.props.scope}] 渲染出错`, error, info.componentStack)
  }

  private handleRetry = (): void => {
    const { onReset, reloadOnReset } = this.props
    this.setState({ error: null })
    onReset?.()
    if (reloadOnReset) window.location.reload()
  }

  private handleOpenLogs = (): void => {
    void invokeCmd<unknown>('app_open_logs').catch(() => {
      // 后端不可用时打开日志也会失败；错误卡片本身已经说明了问题。
    })
  }

  override render(): ReactNode {
    const { error } = this.state
    if (!error) return this.props.children

    return (
      <Panel className="error-boundary" role="alert">
        <PanelHeader
          title={`${this.props.scope}渲染出错`}
          meta="界面已停止渲染以避免白屏；可重试或把日志发给开发者。"
        />
        <p className="inline-message inline-message--error">{errorMessage(error)}</p>
        <div className="inline-message-row">
          <button type="button" className="btn btn--primary" onClick={this.handleRetry}>
            {this.props.reloadOnReset ? '重新加载应用' : '重试'}
          </button>
          <button type="button" className="btn btn--ghost" onClick={this.handleOpenLogs}>
            打开日志目录
          </button>
        </div>
      </Panel>
    )
  }
}
