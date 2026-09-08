import { useEffect, useMemo, useRef, useState } from 'react'
import { localeRevision, t } from '../i18n'
import type { ApprovalDecisionPayload } from '../api'
import type { ModelCatalog, ModelSelection } from '../types'
import { MarkdownText } from '../markdown/MarkdownText'
import type { MarkdownLabels } from '../markdown/MarkdownText'
import { ComposerModelMenu } from './ComposerModelMenu'
import { IconCheck, IconClose, IconPlan } from './icons'

/**
 * 计划审批面板:模型调用 exit_plan 后替换整个输入区。
 * 上层是 markdown 渲染的计划正文,下面是审批区
 * (批准[编辑/完全访问 + 可选执行模型] / 拒绝 + 无边框补充建议),
 * 底部是「退出计划」与「提交审批」。拒绝并填写建议时,模型会
 * 自动按建议重写方案(面板内有标注)。
 */
export function PlanReviewPanel({
  title,
  planText,
  catalog,
  selection,
  onDecision,
  onExit,
}: {
  title?: string
  planText: string
  /** 模型目录;未加载完成时为 null(执行模型选择区隐藏)。 */
  catalog: ModelCatalog | null
  selection: ModelSelection | null
  onDecision: (payload: ApprovalDecisionPayload) => Promise<void>
  onExit: () => void
}) {
  const [choice, setChoice] = useState<'approve' | 'reject'>('approve')
  const [executeMode, setExecuteMode] = useState<'auto-edit' | 'full'>('auto-edit')
  const [model, setModel] = useState<ModelSelection | null>(selection)
  const [modelPicked, setModelPicked] = useState(false)
  const [feedback, setFeedback] = useState('')
  const [submitting, setSubmitting] = useState(false)
  const rootRef = useRef<HTMLDivElement | null>(null)

  // 滚动链拦截:面板出现后滚动只属于面板。从事件目标向上找面板内
  // 能在该方向继续滚的容器,找到就交给原生滚动;全部到边界(或不在
  // 可滚区)时 preventDefault + stopPropagation,防止串到后面的聊天
  // 记录。React 的 onWheel 走 passive 监听,preventDefault 无效,必须
  // 挂原生 { passive: false }。
  useEffect(() => {
    const root = rootRef.current
    if (!root) return
    const onWheel = (event: WheelEvent) => {
      const vertical = Math.abs(event.deltaY) >= Math.abs(event.deltaX)
      const delta = vertical ? event.deltaY : event.deltaX
      if (delta === 0) return
      let node = event.target as HTMLElement | null
      while (node && node !== root) {
        if (node instanceof HTMLElement) {
          const start = vertical ? node.scrollTop : node.scrollLeft
          const max = vertical
            ? node.scrollHeight - node.clientHeight
            : node.scrollWidth - node.clientWidth
          // 该容器在这个方向还能滚:不拦截,交给浏览器原生滚动。
          if (max > 1 && (delta > 0 ? start < max - 1 : start > 1)) return
        }
        node = node.parentElement
      }
      event.preventDefault()
      event.stopPropagation()
    }
    root.addEventListener('wheel', onWheel, { passive: false })
    return () => root.removeEventListener('wheel', onWheel)
  }, [])

  const labels = useMemo<MarkdownLabels>(
    () => ({
      code: { copyLabel: t('copy'), copiedLabel: t('copied') },
      footnotes: t('footnotes'),
    }),
    [localeRevision()],
  )

  const trimmedFeedback = feedback.trim()
  const canSubmit = planText.trim().length > 0 && !submitting

  const submit = async () => {
    if (!canSubmit) return
    setSubmitting(true)
    const payload: ApprovalDecisionPayload =
      choice === 'approve'
        ? {
            decision: 'allow-once',
            executeMode,
            // 只在用户显式换过模型时携带,避免无谓的"已切换"噪音。
            selection: modelPicked && model ? model : undefined,
            feedback: trimmedFeedback || undefined,
          }
        : {
            decision: 'reject',
            feedback: trimmedFeedback || undefined,
          }
    try {
      await onDecision(payload)
    } finally {
      // 决策被服务端结算(approval-decided)后面板由父级卸载;
      // 提交失败(网络/校验)时恢复按钮可再试。
      setSubmitting(false)
    }
  }

  return (
    <div ref={rootRef} className="plan-review-panel">
      <div className="plan-review-head">
        <span className="plan-review-head-icon">
          <IconPlan size={14} />
        </span>
        <span className="plan-review-title">{title?.trim() || t('planReviewTitle')}</span>
        <span className="plan-review-waiting">{t('planReviewWaiting')}</span>
      </div>
      <div className="plan-review-body">
        {planText.trim() ? (
          <MarkdownText text={planText} streaming={false} labels={labels} />
        ) : (
          <div className="plan-review-parse-failed">{t('planReviewParseFailed')}</div>
        )}
      </div>
      <div className="plan-review-actions">
        <div className="plan-review-choice" role="radiogroup" aria-label={t('planReviewTitle')}>
          <button
            type="button"
            role="radio"
            aria-checked={choice === 'approve'}
            className={`plan-review-choice-btn${choice === 'approve' ? ' active' : ''}`}
            onClick={() => setChoice('approve')}
          >
            <IconCheck size={13} />
            {t('planReviewApprove')}
          </button>
          <button
            type="button"
            role="radio"
            aria-checked={choice === 'reject'}
            className={`plan-review-choice-btn${choice === 'reject' ? ' active' : ''}`}
            onClick={() => setChoice('reject')}
          >
            <IconClose size={13} />
            {t('planReviewReject')}
          </button>
        </div>
        {choice === 'approve' && (
          <div className="plan-review-exec">
            <div className="plan-review-modes" role="radiogroup" aria-label={t('planReviewApprove')}>
              <button
                type="button"
                role="radio"
                aria-checked={executeMode === 'auto-edit'}
                className={`plan-review-mode-btn${executeMode === 'auto-edit' ? ' active' : ''}`}
                onClick={() => setExecuteMode('auto-edit')}
              >
                {t('planReviewEditMode')}
              </button>
              <button
                type="button"
                role="radio"
                aria-checked={executeMode === 'full'}
                className={`plan-review-mode-btn${executeMode === 'full' ? ' active' : ''}`}
                onClick={() => setExecuteMode('full')}
              >
                {t('planReviewFullMode')}
              </button>
            </div>
            {model && catalog && catalog.groups.length > 0 && (
              <div className="plan-review-model">
                <span className="plan-review-model-label">{t('planReviewModelLabel')}</span>
                <ComposerModelMenu
                  catalog={catalog}
                  selection={model}
                  disabled={submitting}
                  onChange={(next) => {
                    setModel(next)
                    setModelPicked(true)
                  }}
                />
              </div>
            )}
          </div>
        )}
        <div className="plan-review-feedback">
          <textarea
            className="plan-review-feedback-input"
            value={feedback}
            placeholder={t('planReviewFeedbackPlaceholder')}
            onChange={(event) => setFeedback(event.target.value)}
            rows={2}
          />
          <div className="plan-review-hint">{t('planReviewRewriteHint')}</div>
        </div>
        <div className="plan-review-buttons">
          <button type="button" className="plan-review-exit" onClick={onExit} disabled={submitting}>
            {t('planReviewExit')}
          </button>
          <button
            type="button"
            className="plan-review-submit"
            onClick={() => void submit()}
            disabled={!canSubmit}
          >
            {submitting ? t('planReviewSubmitting') : t('planReviewSubmit')}
          </button>
        </div>
      </div>
    </div>
  )
}
