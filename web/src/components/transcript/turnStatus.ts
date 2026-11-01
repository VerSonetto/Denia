/**
 * 轮次结局的语义归类。
 *
 * 收尾行要回答三个问题,而它们的**视觉分量不该一样**:
 *   1. 这轮怎么结束的?(状态徽标)
 *   2. 为什么?(错误详情 —— 只在出错时才有)
 *   3. 花了多少?(用量)
 *
 * 归类放在组件之外,是为了让"文案"与"配色"读同一套语义:
 * `severity` 决定图标与色档,`kind` 决定文案。组件只做渲染。
 */
import type { TurnEndReason } from '../../generated/core'

/** 结局的严重度:ok 正常收 / warn 没跑完但可继续 / err 真失败。 */
export type TurnSeverity = 'ok' | 'warn' | 'err'

/** 结局种类(与 `TurnEndReason.kind` 对齐,缺字段时归 interrupted)。 */
export type TurnOutcome =
  | 'completed'
  | 'aborted'
  | 'max-tokens'
  | 'interrupted'
  | 'loop-detected'
  | 'error'

/** 失败事实的展示投影(只留收尾行用得到的字段)。 */
export interface TurnFailure {
  code: string
  message: string
  /** HTTP 状态码(提供方报错时才有)。 */
  status?: number
  /** 提供方请求 ID(排障时要用)。 */
  requestId?: string
}

export interface TurnOutcomeInfo {
  severity: TurnSeverity
  kind: TurnOutcome
  failure?: TurnFailure
  /** 死循环的连续重复次数。 */
  repeats?: number
}

/**
 * 把 turn-end 的 reason 归一成结局描述。
 *
 * `reason` 在类型上必填,但运行时未必:旧日志缺字段、后端 `TurnEnd { .. }`
 * 通配落盘的路径都可能给出 undefined。缺 reason 按 interrupted 收 ——
 * 同样是"轮次没有正常跑完",至少界面要说得出话而不是崩。
 */
export function classifyTurn(reason: TurnEndReason | undefined): TurnOutcomeInfo {
  if (!reason) {
    // 崩溃孤儿轮次:被外部打断,结果不完整 —— warn。
    return { severity: 'warn', kind: 'interrupted' }
  }
  switch (reason.kind) {
    case 'completed':
      return { severity: 'ok', kind: 'completed' }
    case 'aborted':
      // 用户自己停的:是预期动作,不是故障 —— 归 warn(中性提示),
      // 不给红色 —— 红色会让"我按了停止"读成"系统出错了"。
      return { severity: 'warn', kind: 'aborted' }
    case 'max-tokens':
      // 触顶:模型输出被截断,这轮的结果不完整 —— warn。
      return { severity: 'warn', kind: 'max-tokens' }
    case 'loop-detected':
      return { severity: 'warn', kind: 'loop-detected', repeats: reason.repeats }
    case 'error': {
      const failure = reason.failure
      return {
        severity: 'err',
        kind: 'error',
        failure: failure
          ? {
              code: failure.code,
              message: failure.message,
              status: failure.status,
              requestId: failure.requestId,
            }
          : undefined,
      }
    }
    case 'interrupted':
    default:
      // 崩溃孤儿轮次:被外部打断,结果不完整 —— warn。
      return { severity: 'warn', kind: 'interrupted' }
  }
}

/**
 * 失败码 → 人话短语。
 *
 * 原始码(`RATE_LIMIT`、`CONTEXT_WINDOW_EXCEEDED`)是给日志看的,不是给
 * 人看的:直接甩在收尾行上,用户还得自己去查它是什么意思。这里把常见的
 * 几个翻成短语,拿不准的**原样返回码** —— 宁可给个准的缩写,也不猜。
 */
const FAILURE_PHRASES: Record<string, string> = {
  NO_ADAPTER: 'failureNoAdapter',
  MISSING_CREDENTIAL: 'failureMissingCredential',
  INVALID_CREDENTIAL: 'failureInvalidCredential',
  AUTH: 'failureAuth',
  RATE_LIMIT: 'failureRateLimit',
  QUOTA: 'failureQuota',
  SERVER: 'failureServer',
  TIMEOUT: 'failureTimeout',
  TRANSPORT: 'failureTransport',
  CONTEXT_WINDOW_EXCEEDED: 'failureContextWindow',
  INVALID_REQUEST: 'failureInvalidRequest',
  EMPTY_RESPONSE: 'failureEmptyResponse',
  UNSUPPORTED_CONTENT: 'failureUnsupportedContent',
  MALFORMED_RESPONSE: 'failureMalformedResponse',
  STREAM_CLOSED: 'failureStreamClosed',
  STEP_LIMIT: 'failureStepLimit',
  BUDGET_EXHAUSTED: 'failureBudgetExhausted',
  SETTINGS: 'failureSettings',
  STORAGE: 'failureStorage',
}

/**
 * 失败码对应的 i18n 键;未知码返回 null(调用方回退到原始码)。
 *
 * 键名带 `failure` 前缀,便于与 `t()` 的键集对齐检查。
 */
export function failurePhraseKey(code: string): string | null {
  return FAILURE_PHRASES[code] ?? null
}
