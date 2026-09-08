import { useEffect, useMemo, useRef, useState } from 'react'
import type { AskAnswer, AskQuestion, AskResolution } from '../types'
import { t } from '../i18n'

/**
 * 模型提问卡片(denia `ask` 工具)。
 *
 * 交互形态与 dsh 的 QuestionComposer 刻意区分:
 * - dsh:一次只显示一道题,底部「上一题/下一题」翻页,答完最后一题才提交;
 * - denia:整组问题同屏(清单式),顶部有进度点,可任意顺序作答,底部一次提交。
 *   理由:dsh 的翻页在大屏上浪费空间、且用户必须按顺序走完才能提交;同屏
 *   清单让"这组问题一共几个、我答了几个"始终可见,也允许先答确定的、再回头
 *   处理需要思考的。
 *
 * 其余差异:推荐项用结构化字段高亮(而非 dsh 的 "(Recommended)" 标签后缀)、
 * 每题独立跳过、自由填写与选项同区并列、等待期显示剩余时间(超时可见)。
 */
export interface AskCardProps {
  requestId: string
  questions: AskQuestion[]
  timeoutMs: number
  /** 请求发起的时刻(epoch ms);用于倒计时。 */
  startedAt: number
  /** 已结算时的结果;存在即渲染只读态。 */
  resolution?: AskResolution
  onSubmit: (answers: AskAnswer[]) => void
  onCancel: () => void
}

type Draft = { selected: string[]; custom: string; skipped: boolean }

function emptyDraft(): Draft {
  return { selected: [], custom: '', skipped: false }
}

function draftAnswered(draft: Draft): boolean {
  return draft.selected.length > 0 || draft.custom.trim() !== ''
}

/** 剩余时间文案:超过 1 分钟显示 mm:ss,否则显示秒。 */
function remainingLabel(ms: number): string {
  const total = Math.max(0, Math.ceil(ms / 1000))
  const minutes = Math.floor(total / 60)
  const seconds = total % 60
  return minutes > 0
    ? `${minutes}:${String(seconds).padStart(2, '0')}`
    : `${seconds}s`
}

function outcomeLabel(outcome: AskResolution['outcome']): string {
  switch (outcome) {
    case 'answered':
      return t('askResolvedAnswered')
    case 'timed-out':
      return t('askResolvedTimeout')
    case 'cancelled':
      return t('askResolvedCancelled')
    default:
      return t('askResolvedUnavailable')
  }
}

/** 只读态:问题与用户答案对照展示(结算后保留在 transcript 里)。 */
function ResolvedCard({
  questions,
  resolution,
}: {
  questions: AskQuestion[]
  resolution: AskResolution
}) {
  const byId = new Map((resolution.answers ?? []).map((answer) => [answer.id, answer]))
  return (
    <div className={`ask-card resolved ${resolution.outcome}`}>
      <div className="ask-resolved-head">
        <span className={`ask-outcome ${resolution.outcome}`}>{outcomeLabel(resolution.outcome)}</span>
        {resolution.reason && <span className="ask-reason">{resolution.reason}</span>}
      </div>
      <ul className="ask-summary">
        {questions.map((question) => {
          const answer = byId.get(question.id)
          const picked = answer?.selected ?? []
          const custom = answer?.custom?.trim() ?? ''
          return (
            <li key={question.id}>
              <span className="ask-summary-q">{question.question}</span>
              <span className="ask-summary-a">
                {answer?.skipped ? (
                  <em>{t('askSkipped')}</em>
                ) : picked.length === 0 && custom === '' ? (
                  <em>{t('askUnanswered')}</em>
                ) : (
                  <>
                    {picked.map((label) => (
                      <span className="ask-chip" key={label}>{label}</span>
                    ))}
                    {custom !== '' && <span className="ask-chip custom">{custom}</span>}
                  </>
                )}
              </span>
            </li>
          )
        })}
      </ul>
    </div>
  )
}

/**
 * 单题编辑器:选项 + 自由填写同区。
 * 与 dsh 的差异:选项是「点一下选中」的按钮列(单选即点即选),多选带勾选框;
 * 自由填写不藏在选项末尾的伪选项里,而是独立一行输入框(有选项时也可用)。
 */
function QuestionBlock({
  question,
  index,
  draft,
  disabled,
  onChange,
}: {
  question: AskQuestion
  index: number
  draft: Draft
  disabled: boolean
  onChange: (next: Draft) => void
}) {
  const options = question.options ?? []
  const multi = question.multiSelect === true
  const allowCustom = question.allowCustom !== false
  const fieldRef = useRef<HTMLTextAreaElement | null>(null)

  const toggle = (label: string) => {
    if (multi) {
      const selected = draft.selected.includes(label)
        ? draft.selected.filter((item) => item !== label)
        : [...draft.selected, label]
      onChange({ ...draft, selected, skipped: false })
    } else {
      // 单选:再点一次取消,方便改主意(自定义文本随之清空)。
      const selected = draft.selected.includes(label) ? [] : [label]
      onChange({ selected, custom: selected.length > 0 ? '' : draft.custom, skipped: false })
    }
  }

  const answered = draftAnswered(draft)

  return (
    <div className={`ask-question${answered ? ' answered' : ''}${draft.skipped ? ' skipped' : ''}`}>
      <div className="ask-question-head">
        <span className="ask-index">{index + 1}</span>
        <div className="ask-heading">
          {question.header && <span className="ask-eyebrow">{question.header}</span>}
          <span className="ask-question-text">{question.question}</span>
          {multi && <span className="ask-tag">{t('askMulti')}</span>}
        </div>
      </div>
      {question.detail && <p className="ask-detail">{question.detail}</p>}
      <div className="ask-options" role={multi ? 'group' : 'radiogroup'}>
        {options.map((option) => {
          const selected = draft.selected.includes(option.label)
          return (
            <button
              type="button"
              key={option.label}
              className={`ask-option${selected ? ' selected' : ''}${option.recommended ? ' recommended' : ''}`}
              role={multi ? 'checkbox' : 'radio'}
              aria-checked={selected}
              disabled={disabled}
              onClick={() => toggle(option.label)}
            >
              <span className={`ask-mark ${multi ? 'box' : 'dot'}`} aria-hidden="true" />
              <span className="ask-option-copy">
                <span className="ask-option-label">
                  {option.label}
                  {option.recommended && <span className="ask-badge">{t('askRecommended')}</span>}
                </span>
                {option.description && (
                  <span className="ask-option-desc">{option.description}</span>
                )}
              </span>
            </button>
          )
        })}
        {allowCustom && (
          <div className="ask-custom">
            <textarea
              ref={fieldRef}
              rows={1}
              value={draft.custom}
              disabled={disabled}
              placeholder={options.length > 0 ? t('askCustomHint') : t('askCustomOnly')}
              onChange={(event) => {
                const custom = event.target.value
                onChange({
                  // 单选填自定义即替换选择;多选保留已勾选项。
                  selected: multi ? draft.selected : [],
                  custom,
                  skipped: false,
                })
              }}
              onKeyDown={(event) => {
                if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
                  event.preventDefault()
                  fieldRef.current?.blur()
                }
              }}
            />
          </div>
        )}
      </div>
      <div className="ask-question-foot">
        <button
          type="button"
          className="ask-skip"
          disabled={disabled}
          onClick={() => onChange({ ...emptyDraft(), skipped: !draft.skipped })}
        >
          {draft.skipped ? t('askUnskip') : t('askSkip')}
        </button>
        <span className="ask-state">
          {draft.skipped
            ? t('askSkipped')
            : answered
              ? t('askAnswered')
              : t('askPending')}
        </span>
      </div>
    </div>
  )
}

/** 模型提问卡片:整组同屏、任意顺序作答、底部一次提交。 */
export function AskCard({
  questions,
  timeoutMs,
  startedAt,
  resolution,
  onSubmit,
  onCancel,
}: AskCardProps) {
  const [drafts, setDrafts] = useState<Draft[]>(() => questions.map(emptyDraft))
  const [now, setNow] = useState(() => Date.now())

  // 倒计时:等待期每 500ms 刷新剩余时间(超时后服务端会结算,卡片转只读)。
  useEffect(() => {
    if (resolution) return
    const timer = window.setInterval(() => setNow(Date.now()), 500)
    return () => window.clearInterval(timer)
  }, [resolution])

  const remaining = timeoutMs - (now - startedAt)
  const expired = remaining <= 0
  const done = useMemo(
    () => drafts.filter((draft) => draftAnswered(draft) || draft.skipped).length,
    [drafts],
  )
  const ready = drafts.every((draft) => draftAnswered(draft) || draft.skipped)

  if (resolution) {
    return <ResolvedCard questions={questions} resolution={resolution} />
  }

  const update = (index: number, next: Draft) => {
    setDrafts((current) => current.map((draft, i) => (i === index ? next : draft)))
  }

  const submit = () => {
    if (!ready || expired) return
    const answers: AskAnswer[] = questions.map((question, index) => {
      const draft = drafts[index]
      const custom = draft.custom.trim()
      if (draft.skipped) {
        return { id: question.id, selected: [], skipped: true }
      }
      return {
        id: question.id,
        // 单选填了自定义时以自定义为准(与后端渲染一致)。
        selected: question.multiSelect === true ? draft.selected : custom === '' ? draft.selected : [],
        ...(custom === '' ? {} : { custom }),
      }
    })
    onSubmit(answers)
  }

  return (
    <div className="ask-card" role="group" aria-label={t('askTitle')}>
      <div className="ask-head">
        <span className="ask-title">{t('askTitle')}</span>
        <span className="ask-progress" title={t('askProgressHint')}>
          {t('askProgress', { done, total: questions.length })}
        </span>
        <span className={`ask-timer${remaining < 30_000 ? ' urgent' : ''}`}>
          {expired ? t('askExpired') : t('askRemaining', { time: remainingLabel(remaining) })}
        </span>
        <button type="button" className="ask-cancel" onClick={onCancel} title={t('askCancelHint')}>
          {t('askCancel')}
        </button>
      </div>
      <div className="ask-body">
        {questions.map((question, index) => (
          <QuestionBlock
            key={question.id}
            question={question}
            index={index}
            draft={drafts[index]}
            disabled={expired}
            onChange={(next) => update(index, next)}
          />
        ))}
      </div>
      <div className="ask-foot">
        <span className="ask-hint">
          {ready ? t('askReadyHint') : t('askNotReadyHint')}
        </span>
        <button
          type="button"
          className="ask-submit"
          disabled={!ready || expired}
          onClick={submit}
        >
          {t('askSubmit')}
        </button>
      </div>
    </div>
  )
}
