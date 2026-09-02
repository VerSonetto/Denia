import { useEffect } from 'react'
import { t } from '../i18n'

/** 一次待审批的工具调用请求。 */
export interface ApprovalRequest {
  /** 工具名(如 bash / edit)。 */
  toolName: string
  /** 调用参数预览(可选)。 */
  argsPreview?: string
}

export type ApprovalDecision = 'reject' | 'once' | 'session'

/**
 * 审批弹窗:AI 调用需要审批的工具时,从**输入框上方**弹出。
 * 两种放行模式:
 * - 仅本次允许:这一次调用放行,下次同工具仍要审批;
 * - 本会话内无需再审批:该工具在本会话内免审;换其他工具仍需重新审批。
 * 当前阶段为前端占位 —— 选择不改变实际执行,弹窗底部注明。
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
        <span className="approval-pop-title">{t('approvalLabel')}</span>
        <span className="approval-pop-tool">{request.toolName}</span>
      </div>
      {request.argsPreview && (
        <p className="approval-pop-desc">
          <code>{request.argsPreview}</code>
        </p>
      )}
      <p className="approval-pop-note">{t('approvalPlaceholderNote')}</p>
      <div className="approval-pop-actions">
        <button type="button" className="approval-btn" onClick={() => onDecide('reject')}>
          {t('approvalReject')}
        </button>
        <button type="button" className="approval-btn" onClick={() => onDecide('once')}>
          {t('approvalOnce')}
        </button>
        <button
          type="button"
          className="approval-btn primary"
          onClick={() => onDecide('session')}
          title={t('approvalSessionHint')}
        >
          {t('approvalSession')}
        </button>
      </div>
    </div>
  )
}
