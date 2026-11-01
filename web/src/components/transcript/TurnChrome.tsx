import { useMemo, useState } from 'react'
import { t } from '../../i18n'
import type { TranscriptNode } from '../../fold'
import { IconAlert, IconCheck, IconInterrupt, IconLoop, IconStop, IconTokenLimit } from '../icons'
import { TurnDiffCard } from '../TurnDiffCard'
import { TurnUsageCard } from '../TurnUsageCard'
import { classifyTurn, failurePhraseKey, type TurnOutcome, type TurnSeverity } from './turnStatus'

/**
 * 轮次收尾行(状态 + 变更卡片)。
 *
 * 这一行回答三件事,**分量各不相同**:
 *   1. 状态徽标 —— 这轮怎么结束的(正常 / 中断 / 触顶 / 死循环 / 出错);
 *   2. 错误详情 —— 只在出错时出现,可展开,默认收起;
 *   3. 用量 —— 始终是量级注脚,点开看明细。
 *
 * 旧版是一条 7px 圆点加纯文字,三档结局(ok/warn/err)只靠文字颜色区分,
 * 且错误文案直接摊在行里 —— 提供方的完整报错能有几百字符,把整行撑成一堵墙。
 * 现在状态升级成有边框的徽标(图标 + 文字 + 色档),错误收进可展开区。
 */
export function TurnChrome({
  node,
  cwd = null,
  onLoopContinue,
}: {
  node: Extract<TranscriptNode, { kind: 'turn-end' }>
  /** 会话头 cwd:产物行里相对路径的锚点。 */
  cwd?: string | null
  /** 死循环提示行点「继续」:自动发送继续消息。 */
  onLoopContinue?: () => void
}) {
  const usage = node.usage
  // reason 来自 turn-end 事件的原始字段。类型上它必填,但**运行时未必**:旧
  // 日志缺字段、以及后端 `TurnEnd { .. }` 通配落盘的路径都可能给出 undefined。
  // classifyTurn 把缺失归为 interrupted(它同样是"轮次没有正常跑完")。
  const outcome = useMemo(() => classifyTurn(node.reason), [node.reason])
  const label = outcomeLabel(outcome.kind)
  const interrupted = outcome.kind !== 'completed'

  return (
    <div className="turn-chrome">
      <div className="turn-reason-row">
        {/* 状态徽标:图标 + 文案。图标承担"是哪一档"的快速识别,文字承担
            精确说法 —— 只有图标时用户得自己回忆图标含义,只有文字时扫读
            一行看不出严重度。 */}
        <span className={`turn-status turn-status-${outcome.severity}`}>
          <StatusIcon outcome={outcome.kind} />
          <span className="turn-status-label">{label}</span>
        </span>
        {outcome.kind === 'loop-detected' && (
          // 死循环保护提示行:「继续」为下划线可点文字,点击自动发送继续消息。
          // 只有死循环需要额外一句说明(为什么停在这),其余结局徽标已说清。
          <span className="reason loop-hint">
            {t('loopDetectedHint')}
            <button
              type="button"
              className="loop-continue"
              onClick={onLoopContinue}
            >
              {t('loopDetectedContinue')}
            </button>
          </span>
        )}
        {usage && (
          // 用量与状态同处一行:状态说"这轮怎么结束的",用量是它的量级注脚。
          // 拆成两行会让两者读作并列的两条信息,而它们本是同一件事。
          // 紧凑计数与底部状态栏同一套语言;点开是明细卡片(缓存读/命中率/推理),
          // 先前这里是个死 tooltip —— 有精确值却看不全。
          <TurnUsageCard usage={usage} />
        )}
      </div>
      {outcome.kind === 'error' && outcome.failure && (
        // 错误详情:默认收起,只露一行摘要(失败码 + 人话短语)。提供方的
        // 完整报错可能几百字符 —— 摊在行里会把整行撑成一堵墙,还会把
        // 「这轮结束了」这条主线淹掉。
        <TurnErrorPanel
          code={outcome.failure.code}
          message={outcome.failure.message}
          status={outcome.failure.status}
          requestId={outcome.failure.requestId}
        />
      )}
      {node.produced !== undefined && node.produced.length > 0 && (
        // 只有"正常完成"才配得上不加限定地读作"这轮改了 N 个文件"。
        // 暂停/报错/触顶/死循环/被打断都可能是半路截断 —— 已落盘的文件
        // 照样要列出来(那是事实),但要让用户看得出这轮没跑完。
        <TurnDiffCard
          files={node.produced}
          cwd={cwd}
          turn={node.turn}
          interrupted={interrupted}
        />
      )}
    </div>
  )
}

/** 结局 → 徽标文案。 */
function outcomeLabel(kind: TurnOutcome): string {
  switch (kind) {
    case 'completed':
      return t('reasonCompleted')
    case 'aborted':
      return t('reasonAborted')
    case 'max-tokens':
      return t('reasonMaxTokens')
    case 'loop-detected':
      return t('reasonLoopDetected')
    case 'error':
      return t('reasonError')
    case 'interrupted':
    default:
      return t('reasonInterrupted')
  }
}

/** 结局 → 图标。图标是小尺寸下唯一能快速分档的通道。 */
function StatusIcon({ outcome }: { outcome: TurnOutcome }) {
  switch (outcome) {
    case 'completed':
      return <IconCheck size={11} />
    case 'aborted':
      return <IconStop size={11} />
    case 'max-tokens':
      return <IconTokenLimit size={11} />
    case 'loop-detected':
      return <IconLoop size={11} />
    case 'error':
      return <IconAlert size={11} />
    case 'interrupted':
    default:
      return <IconInterrupt size={11} />
  }
}

/**
 * 错误详情面板。
 *
 * 收起态只给"失败码 + 人话短语"一行 —— 这是用户唯一需要读到的信息
 * ("限流了"、"上下文超了")。展开后才给提供方的原始报文、HTTP 状态与
 * 请求 ID(排障三件套)。
 *
 * 失败码先查词表翻成人话;词表里没有就原样显示 —— 猜一个近似含义
 * 比给准确的缩写更糟。
 */
function TurnErrorPanel({
  code,
  message,
  status,
  requestId,
}: {
  code: string
  message: string
  status?: number
  requestId?: string
}) {
  const [open, setOpen] = useState(false)
  const phraseKey = failurePhraseKey(code)
  const phrase = phraseKey ? t(phraseKey as Parameters<typeof t>[0]) : code

  return (
    <div className={`turn-error${open ? ' open' : ''}`}>
      <button
        type="button"
        className="turn-error-summary"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        <span className="turn-error-code">{code}</span>
        <span className="turn-error-phrase">{phrase}</span>
        <span className="turn-error-toggle">{open ? t('turnErrorHide') : t('turnErrorShow')}</span>
      </button>
      {open && (
        <div className="turn-error-body">
          {message && <p className="turn-error-message">{message}</p>}
          <dl className="turn-error-facts">
            {status !== undefined && (
              <div>
                <dt>{t('turnErrorStatus')}</dt>
                <dd className="stats-mono">{status}</dd>
              </div>
            )}
            {requestId && (
              <div>
                <dt>{t('turnErrorRequestId')}</dt>
                <dd className="stats-mono">{requestId}</dd>
              </div>
            )}
          </dl>
        </div>
      )}
    </div>
  )
}

/** 严重度 → CSS 档位(供外部样式钩子使用)。 */
export type { TurnSeverity }
