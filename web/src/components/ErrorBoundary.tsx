import { Component, type ErrorInfo, type ReactNode } from 'react'
import { t } from '../i18n'

/**
 * 渲染错误边界:兜住子树里抛出的渲染期异常。
 *
 * 为什么必须有:React 18 遇到未捕获的渲染异常会**卸载整棵根树**——
 * 界面直接变白屏,用户连"哪里坏了"都看不到(denia 曾因此整页白掉:
 * Transcript 的 hook 顺序违规,见 `check-hooks.mjs` 门禁)。渲染期异常
 * 无法用 try/catch 捕获,边界是唯一的兜底手段。
 *
 * 分级使用:根节点套一层(任何组件崩了都不至于整页空白),高风险子树
 * (transcript / 轨迹 / markdown 渲染)再各自套一层——局部崩了不该带走
 * 侧栏、输入框这些无关区域。
 */
export class ErrorBoundary extends Component<
  {
    children: ReactNode
    /** 降级视图的标题;缺省用通用文案。 */
    title?: string
    /** 出错时回调(供上层上报/标记)。 */
    onError?: (error: Error, info: ErrorInfo) => void
  },
  { error: Error | null; detailOpen: boolean }
> {
  state: { error: Error | null; detailOpen: boolean } = { error: null, detailOpen: false }

  static getDerivedStateFromError(error: Error) {
    return { error, detailOpen: false }
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // 保留到控制台:边界只负责不白屏,错误本身要看得见(排查/上报依据)。
    console.error('[render-error]', error, info.componentStack)
    this.props.onError?.(error, info)
  }

  private retry = () => {
    // 重置边界让子树重新挂载;若错误是确定性的会立刻再触发一次。
    this.setState({ error: null, detailOpen: false })
  }

  private reload = () => {
    window.location.reload()
  }

  render() {
    const { error } = this.state
    if (error === null) return this.props.children
    return (
      <div className="render-error" role="alert">
        <div className="render-error-title">{this.props.title ?? t('renderErrorTitle')}</div>
        <p className="render-error-desc">{t('renderErrorDesc')}</p>
        <div className="render-error-actions">
          <button type="button" className="render-error-btn" onClick={this.retry}>
            {t('renderErrorRetry')}
          </button>
          <button type="button" className="render-error-btn" onClick={this.reload}>
            {t('renderErrorReload')}
          </button>
        </div>
        <details
          className="render-error-details"
          open={this.state.detailOpen}
          onToggle={(event) =>
            this.setState({ detailOpen: (event.target as HTMLDetailsElement).open })
          }
        >
          <summary>{t('renderErrorDetail')}</summary>
          <pre>{error.message}</pre>
        </details>
      </div>
    )
  }
}
