import { useEffect, useMemo, useRef, useState } from 'react'
import type { AskAnswer, AskQuestion, AskResolution } from '../types'
import { t } from '../i18n'

/**
 * 模型提问卡片(denia `ask` 工具)。
 *
 * 交互形态(对照 dsh 的 QuestionComposer,分页思路一致):
 * - 一次只显示一道题:顶部题目导航(点序号直接跳题)、底部「上一题 / 下一题」;
 * - 单选选中、或点「跳过本题」后自动前进到下一题,一路答下去不必回点翻页;
 * - 每题仍可独立跳过,全部处理完(作答或跳过)才允许提交。
 *
 * 为什么分页:一组最多 8 道题,每题还带选项与自由填写,同屏平铺会把卡片撑得
 * 很长,"当前答到第几题"的焦点也散了。分页把单题放到视野中心,再用顶部导航
 * 保住全局感——"一共几题、答了几题、哪题跳过了"在导航条上一眼可见。
 *
 * 其余与 dsh 的差异:推荐项用结构化字段高亮(而非 dsh 的 "(Recommended)" 标签
 * 后缀)、每题独立跳过、自由填写与选项同区并列、等待期显示剩余时间(超时可见)。
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
  onAdvance,
}: {
  question: AskQuestion
  index: number
  draft: Draft
  disabled: boolean
  onChange: (next: Draft) => void
  /** 本题处理完(单选选中 / 跳过)后的前进动作;分页模式下切到下一题。 */
  onAdvance: () => void
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
      return
    }
    // 单选:再点一次取消,方便改主意(自定义文本随之清空)。
    const selected = draft.selected.includes(label) ? [] : [label]
    onChange({ selected, custom: selected.length > 0 ? '' : draft.custom, skipped: false })
    // 选中即视为答完本题,自动前进;取消选择不前进(多选无法判定"答完")。
    if (selected.length > 0) onAdvance()
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
          onClick={() => {
            const skipped = !draft.skipped
            onChange({ ...emptyDraft(), skipped })
            // 跳过也算处理完本题,同样自动前进;取消跳过则留在原地。
            if (skipped) onAdvance()
          }}
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

/** 模型提问卡片:分页作答,全部处理完后一次提交。 */
export function AskCard({
  questions,
  timeoutMs,
  startedAt,
  resolution,
  onSubmit,
  onCancel,
}: AskCardProps) {
  const [drafts, setDrafts] = useState<Draft[]>(() => questions.map(emptyDraft))
  const [page, setPage] = useState(0)
  const [now, setNow] = useState(() => Date.now())
  const bodyRef = useRef<HTMLDivElement | null>(null)
  // 只有用户主动切题(点序号/翻页/作答后自动前进)才把焦点交给题目区;
  // 首次渲染不算——卡片是随消息流出现的,抢焦点会把页面拽到卡片上。
  // 用「意图标记」而不是「是否挂载过」:StrictMode 会双跑 effect,
  // 后者在开发模式下会把首帧误判成切题。
  const focusPending = useRef(false)

  const total = questions.length
  // 页码夹在题目范围内:题目集合不会中途变短,这里只是兜底。
  const current = Math.max(0, Math.min(page, total - 1))

  // 倒计时:等待期每 500ms 刷新剩余时间(超时后服务端会结算,卡片转只读)。
  useEffect(() => {
    if (resolution) return
    const timer = window.setInterval(() => setNow(Date.now()), 500)
    return () => window.clearInterval(timer)
  }, [resolution])

  // 切题:题目区滚回顶部;若这次切换由用户操作触发,再把焦点交给题目区——
  // 键盘作答自动前进后焦点不会掉到 body 上,可以接着按 Tab 选下一题的选项。
  useEffect(() => {
    const element = bodyRef.current
    if (!element) return
    element.scrollTop = 0
    if (!focusPending.current) return
    focusPending.current = false
    element.focus({ preventScroll: true })
  }, [current])

  const remaining = timeoutMs - (now - startedAt)
  const expired = remaining <= 0
  const done = useMemo(
    () => drafts.filter((draft) => draftAnswered(draft) || draft.skipped).length,
    [drafts],
  )
  const ready = done === total

  if (resolution) {
    return <ResolvedCard questions={questions} resolution={resolution} />
  }
  // 后端保证至少一题;空数组直接不渲染,避免下面的索引越界。
  if (total === 0) return null

  const update = (index: number, next: Draft) => {
    setDrafts((list) => list.map((draft, i) => (i === index ? next : draft)))
  }

  /** 切到指定题(点导航序号 / 翻页按钮 / 作答后自动前进)。
   *  停在原地时不置焦点标记,免得留给下一次切换去抢焦点。 */
  const goTo = (index: number) => {
    const target = Math.max(0, Math.min(index, total - 1))
    if (target === current) return
    focusPending.current = true
    setPage(target)
  }

  /** 处理完本题后前进;最后一题停在原地,由用户点提交。 */
  const advance = () => goTo(current + 1)

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
          {t('askProgress', { done, total })}
        </span>
        <span className={`ask-timer${remaining < 30_000 ? ' urgent' : ''}`}>
          {expired ? t('askExpired') : t('askRemaining', { time: remainingLabel(remaining) })}
        </span>
        <button type="button" className="ask-cancel" onClick={onCancel} title={t('askCancelHint')}>
          {t('askCancel')}
        </button>
      </div>
      {/* 题目导航:分页后仍要一眼看见"共几题、答到哪、哪题跳过了"。
          单题时没有可跳的目标,整条隐藏。 */}
      {total > 1 && (
        <div className="ask-nav" role="group" aria-label={t('askNavLabel')}>
          {questions.map((question, index) => {
            const draft = drafts[index]
            const state = draft.skipped ? ' skipped' : draftAnswered(draft) ? ' answered' : ''
            return (
              <button
                key={question.id}
                type="button"
                className={`ask-nav-item${state}${index === current ? ' current' : ''}`}
                aria-current={index === current ? 'step' : undefined}
                aria-label={t('askNavJump', { index: index + 1 })}
                title={question.header ?? question.question}
                onClick={() => goTo(index)}
              >
                {index + 1}
              </button>
            )
          })}
        </div>
      )}
      {/* tabIndex=-1:仅供程序化聚焦(切题后接管焦点),不进 Tab 序列。 */}
      <div className="ask-body" ref={bodyRef} tabIndex={-1}>
        <QuestionBlock
          key={questions[current].id}
          question={questions[current]}
          index={current}
          draft={drafts[current]}
          disabled={expired}
          onChange={(next) => update(current, next)}
          onAdvance={advance}
        />
      </div>
      {total > 1 && (
        <div className="ask-pager">
          <button type="button" disabled={current === 0} onClick={() => goTo(current - 1)}>
            {t('askPrev')}
          </button>
          <span className="ask-page">{t('askPage', { index: current + 1, total })}</span>
          <button
            type="button"
            disabled={current === total - 1}
            onClick={() => goTo(current + 1)}
          >
            {t('askNext')}
          </button>
        </div>
      )}
      <div className="ask-foot">
        <span className="ask-hint">
          {ready ? t('askReadyHint') : t('askNotReadyHint', { count: total - done })}
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
