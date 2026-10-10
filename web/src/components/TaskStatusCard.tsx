import { useState } from 'react'
import type { EvidenceSource, LedgerOverflow, RequirementKind, StaleReason } from '../types'
import type { TaskCheckRun, TaskSnapshot } from '../api'
import { t } from '../i18n'

/** 账本图标(带勾的清单)。 */
function IconLedger({ size = 14 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" fill="none" aria-hidden>
      <rect x="2.6" y="2.2" width="10.8" height="11.6" rx="2" stroke="currentColor" strokeWidth="1.3" />
      <path d="M5.4 8.2 7 9.8l3.6-3.8" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

function IconChevron({ open }: { open: boolean }) {
  return (
    <svg width={12} height={12} viewBox="0 0 16 16" fill="none" aria-hidden
      style={{ transform: open ? 'rotate(90deg)' : undefined }}>
      <path d="M6 3.5 10.5 8 6 12.5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

function statusLabel(status: TaskSnapshot['status']): string {
  switch (status) {
    case 'active':
      return t('taskStatusActive')
    case 'blocked':
      return t('taskStatusBlocked')
    case 'closed':
      return t('taskStatusClosed')
  }
}

function outcomeLabel(outcome: NonNullable<TaskSnapshot['outcome']>): string {
  switch (outcome) {
    case 'unverified':
      return t('taskOutcomeUnverified')
    case 'passed':
      return t('taskOutcomePassed')
    case 'failed':
      return t('taskOutcomeFailed')
    case 'blocked':
      return t('taskOutcomeBlocked')
  }
}

function kindLabel(kind: RequirementKind): string {
  switch (kind) {
    case 'goal':
      return t('taskKindGoal')
    case 'task-type':
      return t('taskKindTaskType')
    case 'constraint':
      return t('taskKindConstraint')
    case 'acceptance':
      return t('taskKindAcceptance')
  }
}

/** "为什么不是通过":服务端的裁决给原因,这里只负责把它说成人话。 */
function staleReasonText(reason: StaleReason): string {
  switch (reason.kind) {
    case 'never-run':
      return t('taskReasonNeverRun')
    case 'revision-changed':
      return t('taskReasonRevisionChanged')
    case 'inputs-changed': {
      // 路径可能很多,截断但报出被截掉的条数(不能把"改了几个文件"藏起来)。
      const shown = reason.paths.slice(0, 5).join(', ')
      const hidden = reason.paths.length - 5
      return t('taskReasonInputsChanged', { paths: hidden > 0 ? `${shown} +${hidden}` : shown })
    }
    case 'no-check-run':
      return t('taskReasonNoCheckRun')
    case 'foreign-origin':
      return t('taskReasonForeignOrigin')
    case 'revision-conflict':
      return t('taskReasonRevisionConflict')
  }
}

function sourceLabel(source: EvidenceSource): string {
  switch (source.kind) {
    case 'tool-call':
      return t('taskSourceToolCall', { tool: source.tool })
    case 'check':
      return t('taskSourceCheck', { command: source.command })
    case 'file-read':
      return t('taskSourceFileRead', { path: source.path })
  }
}

function checkResultText(check: TaskCheckRun): string {
  const exit = check.exitCode === undefined ? t('taskCheckNotRun') : String(check.exitCode)
  return `${exit} · ${t('taskCheckExpect', { code: check.expectExitCode })}`
}

/** 有界保存丢弃了什么:每个非零计数都要出现在汇总里。 */
function droppedParts(overflow: LedgerOverflow): string[] {
  const parts: Array<[number, string]> = [
    [overflow.requirements, t('taskOverflowRequirements')],
    [overflow.facts, t('taskOverflowFacts')],
    [overflow.assumptions, t('taskOverflowAssumptions')],
    [overflow.failedAttempts, t('taskOverflowFailedAttempts')],
    [overflow.openQuestions, t('taskOverflowOpenQuestions')],
    [overflow.evidence, t('taskOverflowEvidence')],
    [overflow.validations, t('taskOverflowValidations')],
    [overflow.changes, t('taskOverflowChanges')],
    [overflow.remaining, t('taskOverflowRemaining')],
    [overflow.revisions, t('taskOverflowRevisions')],
  ]
  return parts
    .filter(([count]) => count > 0)
    .map(([count, label]) => t('taskOverflowItem', { label, count }))
}

/**
 * 任务账本卡片(只读投影)。
 *
 * 与 GoalBar 同一视觉语言:composer-stack 内的圆角卡片,收起时一行摘要,
 * 展开后是明细。摘要行必须能独立回答"收口了吗、凭什么" —— 所以裁决与不通过
 * 的原因不藏在展开区里。
 *
 * 这里**没有**任何写入口:状态由模型用 update_task 声明,验证结论只能由
 * run_checks 跑出来;卡片只画服务端折叠出来的结论。`task === null`(会话里
 * 没有任务事件)时整张卡片不渲染。
 */
export function TaskStatusCard({ task }: { task: TaskSnapshot | null }) {
  const [expanded, setExpanded] = useState(false)
  if (!task) return null

  const verification = task.verification
  const current = task.requirements.filter((item) => item.revision === task.revision)
  const acceptance = current.filter((item) => item.kind === 'acceptance')
  const olderCount = task.requirements.length - current.length
  const covered = new Set(verification.kind === 'verified' ? verification.covered : [])
  const coveredCount = acceptance.filter((item) => covered.has(item.id)).length
  const verdictKind = verification.kind === 'verified' ? verification.verdict : 'unverified'
  const verdictText =
    verification.kind === 'verified'
      ? verification.verdict === 'passed'
        ? t('taskVerificationPassed')
        : t('taskVerificationFailed')
      : staleReasonText(verification.reason)
  // 生效的那条结论:判据与折叠层一致(绑定当前 revision 的那条,`at` 定位)。
  const effective =
    verification.kind === 'verified'
      ? task.validations.find((item) => item.revision === task.revision && item.at === verification.at)
      : undefined
  const revisionLabel = (id: string): string => {
    const ordinal = task.revisions.find((item) => item.id === id)?.ordinal
    return ordinal === undefined ? id : t('taskRevisionOrdinal', { ordinal })
  }
  const dropped = droppedParts(task.overflow)
  const dirty = task.revisionConflict || task.revisionRejected > 0

  return (
    <div
      className="task-card"
      data-task-card=""
      data-task-status={task.status}
      data-task-verification={verification.kind}
    >
      <button
        type="button"
        className="task-card-summary"
        data-task-card-toggle=""
        aria-expanded={expanded}
        title={expanded ? t('taskCardToggleClose') : t('taskCardToggleOpen')}
        onClick={() => setExpanded((value) => !value)}
      >
        <span className="task-icon">
          <IconLedger size={14} />
        </span>
        <span className="task-status-label" data-task-status-label="">{statusLabel(task.status)}</span>
        <span className="task-goal" data-task-goal="" title={task.goal}>
          {task.goal}
        </span>
        {task.outcome && (
          <span className="task-outcome-pill" data-task-outcome={task.outcome}>
            {outcomeLabel(task.outcome)}
          </span>
        )}
        <span className="task-verdict" data-task-verdict={verdictKind} title={verdictText}>
          {verdictText}
        </span>
        <span className="task-spacer" />
        <span className="task-chevron">
          <IconChevron open={expanded} />
        </span>
      </button>

      {expanded && (
        <div className="task-details" data-task-details="">
          {dirty && (
            <p className="task-warn" data-task-revision-dirty={task.revisionConflict ? 'conflict' : 'rejected'}>
              {t('taskReasonRevisionConflict')}
              {task.revisionRejected > 0 ? ` · ${t('taskRevisionConflicts', { count: task.revisionRejected })}` : ''}
            </p>
          )}
          {task.status === 'blocked' && task.blockedReason && (
            <p className="task-warn" data-task-blocked-reason="">
              {t('taskBlockedReason', { reason: task.blockedReason })}
            </p>
          )}

          <section className="task-section">
            <h4 className="task-section-title">
              {t('taskSectionRequirements')}
              {acceptance.length > 0 && (
                <span className="task-section-meta" data-task-coverage={`${coveredCount}/${acceptance.length}`}>
                  {t('taskCoverage', { covered: coveredCount, total: acceptance.length })}
                </span>
              )}
            </h4>
            {current.length === 0 ? (
              <p className="task-empty">{t('taskNoAcceptance')}</p>
            ) : (
              <ul className="task-list">
                {current.map((item) => (
                  <li key={item.id} className="task-item" data-task-requirement={item.id}>
                    <span className="task-kind-chip">{kindLabel(item.kind)}</span>
                    <span className="task-text">{item.text}</span>
                    {item.kind === 'acceptance' && (
                      <span
                        className={covered.has(item.id) ? 'task-covered' : 'task-uncovered'}
                        data-task-covered={covered.has(item.id) ? 'yes' : 'no'}
                      >
                        {covered.has(item.id) ? t('taskCovered') : t('taskNotCovered')}
                      </span>
                    )}
                  </li>
                ))}
              </ul>
            )}
            {olderCount > 0 && (
              <p className="task-muted" data-task-older-requirements={String(olderCount)}>
                {t('taskOlderRequirements', { count: olderCount })}
              </p>
            )}
          </section>

          <section className="task-section">
            <h4 className="task-section-title">{t('taskSectionVerification')}</h4>
            <p className="task-verdict-line" data-task-verdict-line={verdictKind}>
              {verdictText}
            </p>
            {verification.kind === 'verified' && (
              <p className="task-muted" data-task-verdict-fingerprints={String(verification.fingerprints.length)}>
                {t('taskEvidenceFingerprints', { count: verification.fingerprints.length })}
              </p>
            )}
            {effective && effective.checks.length > 0 && (
              <ul className="task-list">
                {effective.checks.map((check, index) => (
                  <li key={`${check.command}-${index}`} className="task-check-row" data-task-check-passed={check.passed ? 'yes' : 'no'}>
                    <code className="task-command">{check.command}</code>
                    <span className={check.passed ? 'task-covered' : 'task-uncovered'}>{checkResultText(check)}</span>
                    {check.workdir && (
                      <span className="task-muted">{t('taskCheckWorkdir', { workdir: check.workdir })}</span>
                    )}
                  </li>
                ))}
              </ul>
            )}
            <p className="task-meta" data-task-validation-count={String(task.validations.length)}>
              {t('taskValidationsCount', { count: task.validations.length })}
            </p>
            {task.validations.length > 0 && (
              <ul className="task-list">
                {task.validations.map((item, index) => (
                  <li
                    key={`${item.revision}-${item.at}-${index}`}
                    className="task-item"
                    data-task-validation={item === effective ? 'effective' : 'audit'}
                  >
                    <span className="task-source">{revisionLabel(item.revision)}</span>
                    <span className="task-text">
                      {item.verdict === 'passed'
                        ? t('taskVerificationPassed')
                        : item.verdict === 'failed'
                          ? t('taskVerificationFailed')
                          : t('taskCheckNotRun')}
                    </span>
                    <span className="task-muted">
                      {t('taskValidationChecks', { count: item.checks.length })}
                      {item.originSession ? ` · ${t('taskEvidenceOrigin', { session: item.originSession })}` : ''}
                    </span>
                    {item === effective && (
                      <span className="task-effective-chip">{t('taskValidationEffective')}</span>
                    )}
                  </li>
                ))}
              </ul>
            )}
          </section>

          <section className="task-section">
            <h4 className="task-section-title">{t('taskSectionOpenQuestions')}</h4>
            {task.openQuestions.length === 0 ? (
              <p className="task-empty">{t('taskNone')}</p>
            ) : (
              <ul className="task-list">
                {task.openQuestions.map((item) => (
                  <li key={item.id} className="task-item" data-task-open-question={item.id}>
                    <span className="task-text">{item.text}</span>
                  </li>
                ))}
              </ul>
            )}
          </section>

          <section className="task-section">
            <h4 className="task-section-title">{t('taskSectionRemaining')}</h4>
            {task.remaining.length === 0 ? (
              <p className="task-empty">{t('taskNone')}</p>
            ) : (
              <ul className="task-list">
                {task.remaining.map((text, index) => (
                  // 待完成整表替换且条目无身份:下标是这里唯一稳定的 key。
                  // eslint-disable-next-line react/no-array-index-key
                  <li key={`${index}-${text}`} className="task-item" data-task-remaining={String(index)}>
                    <span className="task-text">{text}</span>
                  </li>
                ))}
              </ul>
            )}
          </section>

          <section className="task-section">
            <h4 className="task-section-title">
              {t('taskSectionEvidence')}
              <span className="task-section-meta" data-task-evidence-count={String(task.evidence.length)}>
                {t('taskEvidenceCount', { count: task.evidence.length })}
              </span>
            </h4>
            {task.evidence.length === 0 ? (
              <p className="task-empty">{t('taskNoEvidence')}</p>
            ) : (
              <ul className="task-list">
                {task.evidence.map((item) => (
                  <li key={item.id} className="task-item" data-task-evidence={item.id}>
                    <span className="task-source">{sourceLabel(item.source)}</span>
                    <span className="task-text">{item.summary}</span>
                    <span className="task-muted">
                      {t('taskEvidenceFingerprints', { count: item.fingerprintCount })}
                      {item.originSession ? ` · ${t('taskEvidenceOrigin', { session: item.originSession })}` : ''}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </section>

          <p className="task-meta" data-task-counts="">
            {t('taskCounts', {
              facts: task.counts.facts,
              assumptions: task.counts.assumptions,
              failedAttempts: task.counts.failedAttempts,
              changes: task.counts.changes,
            })}
          </p>
          {dropped.length > 0 && (
            <p className="task-warn" data-task-overflow={String(dropped.length)}>
              {t('taskOverflow', { summary: dropped.join(' · ') })}
            </p>
          )}
        </div>
      )}
    </div>
  )
}
