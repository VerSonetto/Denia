import { useEffect } from 'react'
import { t } from '../i18n'

/** 一次待审批的工具调用请求。 */
export interface ApprovalRequest {
  /** 后端请求 id,用于 POST 应答。 */
  requestId: string
  /** 工具名(如 bash / edit)。 */
  toolName: string
  /** 调用参数预览(可选)。 */
  argsPreview?: string
  /** 工具给出的升权理由。 */
  reason?: string
}

export type ApprovalDecision = 'reject' | 'allow-once' | 'allow-session'

/**
 * 审批弹窗:AI 请求越出当前权限模式时,从输入框上方弹出。
 * 三种决策:拒绝、本窗口放行(本会话内同类不再询问)、允许一次;
 * 计划审批(exit_plan)走 PlanReviewPanel,不用这里的三选。
 */
export function ApprovalDialog({
  request,
  onDecide,
}: {
  request: ApprovalRequest | null
  onDecide: (decision: ApprovalDecision) => void
}) {
  useEffect(() => {
    if (!request) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onDecide('reject')
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [request, onDecide])

  if (!request) return null

  return (
    <div className="approval-pop" role="dialog" aria-modal="false" aria-label={t('approvalLabel')}>
      <div className="approval-pop-head">
        <span className="approval-pop-title">{t('approvalWaiting')}</span>
        <span className="approval-pop-tool">{request.toolName}</span>
      </div>
      {request.reason && <p className="approval-pop-reason">{request.reason}</p>}
      {request.argsPreview && (
        <p className="approval-pop-desc">
          <code>{request.argsPreview}</code>
        </p>
      )}
      <div className="approval-pop-actions">
        <button type="button" className="approval-btn" onClick={() => onDecide('reject')}>
          {t('approvalReject')}
        </button>
        <button type="button" className="approval-btn" onClick={() => onDecide('allow-session')}>
          {t('approvalAllowSession')}
        </button>
        <button
          type="button"
          className="approval-btn primary"
          onClick={() => onDecide('allow-once')}
        >
          {t('approvalAllowOnce')}
        </button>
      </div>
    </div>
  )
}
