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

export type ApprovalDecision = 'reject' | 'allow-once'

/**
 * 审批弹窗:AI 请求沙箱升权时,从输入框上方弹出(抄 dsh ApprovalPanel)。
 * 只有两种决策:拒绝、允许一次。没有“本会话免审”——dsh 的审批只作用于
 * 这一次调用;要长期放行请切换更高的权限预设。
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
