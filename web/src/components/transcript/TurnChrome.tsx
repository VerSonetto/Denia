import { t } from '../../i18n'
import type { TranscriptNode } from '../../fold'
import { TurnDiffCard } from '../TurnDiffCard'
import { TurnUsageCard } from '../TurnUsageCard'

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
  // reason 来自 turn-end 事件的原始字段。类型上它必填,但**运行时未必**:旧
  // 日志缺字段、以及后端 `TurnEnd { .. }` 通配落盘的路径都可能给出 undefined,
  // 而这里原本一次读了六遍 `reason.kind` —— 缺一次就整块 transcript 抛
  // Cannot read properties of undefined (reading 'kind')。缺失按 interrupted
  // 收:它同样是"轮次没有正常跑完",至少界面要说得出话而不是崩。
  const usage = node.usage
  const reason = node.reason ?? { kind: 'interrupted' }
  const label =
    reason.kind === 'completed'
      ? t('reasonCompleted')
      : reason.kind === 'aborted'
        ? t('reasonAborted')
        : reason.kind === 'max-tokens'
          ? t('reasonMaxTokens')
          : reason.kind === 'interrupted'
            ? t('reasonInterrupted')
            : reason.kind === 'loop-detected'
              ? t('loopDetectedHint')
              : `${t('reasonError')}: [${reason.failure?.code ?? 'unknown'}] ${reason.failure?.message ?? ''}`
  const cls =
    reason.kind === 'completed' ? '' : reason.kind === 'error' ? 'err' : 'warn'
  return (
    <div className="turn-chrome">
      <div className="turn-reason-row">
        <span
          className={`dot ${reason.kind === 'completed' ? 'ok' : reason.kind === 'error' ? 'err' : 'run'}`}
        />
        {reason.kind === 'loop-detected' ? (
          // 死循环保护提示行:「继续」为下划线可点文字,点击自动发送继续消息。
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
        ) : (
          <span className={`reason ${cls}`} title={label}>
            {label}
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
      {node.produced !== undefined && node.produced.length > 0 && (
        // 只有"正常完成"才配得上不加限定地读作"这轮改了 N 个文件"。
        // 暂停/报错/触顶/死循环/被打断都可能是半路截断 —— 已落盘的文件
        // 照样要列出来(那是事实),但要让用户看得出这轮没跑完。
        <TurnDiffCard
          files={node.produced}
          cwd={cwd}
          turn={node.turn}
          interrupted={reason.kind !== 'completed'}
        />
      )}
    </div>
  )
}
