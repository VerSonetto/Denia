import { useMemo, useState } from 'react'
import { localeRevision, t } from '../../i18n'
import type { BashExecution, BashOutput } from '../../toolDisplay'
import {
  bashCommand,
  bashExecution,
  bashFailed,
  bashOutputParts,
  BASH_END_REASON_LABELS,
  cleanToolOutput,
  EMPTY_BASH_EXECUTION,
  EMPTY_BASH_OUTPUT,
  firstLine,
  liveBashOutput,
  parseArgsObject,
  skillLoadBody,
  toolCallInput,
  toolCallSummary,
  toolDiff,
  todoSnapshot,
} from '../../toolDisplay'
import { toolMeta } from './toolMeta'
import { MarkdownText } from '../../markdown/MarkdownText'
import type { MarkdownLabels } from '../../markdown/MarkdownText'
import type { TranscriptNode } from '../../fold'
import type { AskAnswer } from '../../types'
import { useFileOpen } from '../../fileOpen'
import { DiffCard, DiffStat } from '../DiffCard'
import { TodoCard } from '../TodoCard'
import { BashCard } from '../BashCard'
import { AskCard } from '../AskCard'
import { IconToolAsk } from '../icons'

/**
 * bash 的执行徽标:**只在需要说话时才出现**。
 *
 * 成功的命令不需要一枚对勾来证明自己 —— shell 的语义就是"没消息就是好
 * 消息",而且一个绿勾钉在命令行末尾既突兀又占地方。非 0 才需要说话,写
 * 法是 `exit 3`:等宽小字、错误前景色、无底色(底色会让人误以为是工具
 * 失败,而非零退出是命令自己的结果)。
 *
 * 超时 / 中断 / 没起来这三类情况**根本没有退出码**(文案首行的 `-1` 只是
 * 给模型看的哨兵),这时报结束原因,不编一个数字出来。
 */
function ExitBadge({ code, reason }: BashExecution) {
  if (code !== undefined && code !== 0) {
    return (
      <span className="exit-badge" title={t('bashExitNonZero', { code })}>
        exit {code}
      </span>
    )
  }
  const label = reason ? BASH_END_REASON_LABELS[reason] : undefined
  // 退出码拿到了(0)就没什么可说的;旧数据没有 reason,这里自然落空。
  if (code !== undefined || !label) return null
  return (
    <span className="exit-badge" title={t('bashEndHint')}>
      {t(label)}
    </span>
  )
}

export function ToolRow({
  node,
  onAskAnswer,
  onAskCancel,
}: {
  node: Extract<TranscriptNode, { kind: 'tool' }>
  onAskAnswer?: (requestId: string, answers: AskAnswer[]) => void
  onAskCancel?: (requestId: string) => void
}) {
  const [open, setOpen] = useState(false)
  const running = !node.result
  const meta = toolMeta(node.name)
  // 文件类工具(edit/write_file)整行围绕"改了哪个文件、改了几行"组织:
  // 头部显示文件名,右侧挂 diff 增删徽标,展开后是 diff 本身。
  // 按 args/结果缓存:流式补参数时只在引用变化时重算。
  const diff = useMemo(() => toolDiff(node.name, node.args, node.result?.content), [
    node.name,
    node.args,
    node.result?.content,
  ])
  // todo_write:清单卡片(整表替换,只突出本次变化项)。
  const todos = useMemo(
    () => (node.name === 'todo_write' ? todoSnapshot(node.args, node.prevTodos) : null),
    [node.name, node.args, node.prevTodos],
  )
  const argSummary = toolCallSummary(node.name, node.args)
  // 头部摘要:文件类工具一律显示文件名(不是 old_string/content 原文);
  // todo_write 走 toolCallSummary 里的进度分支,同样不出现 JSON。
  const headSummary = diff ? diff.path : argSummary
  // 文件类工具(edit / write_file)的**完整路径**:头部只显示短名(shortPath),
  // 打开文件需要完整相对路径。参数解析失败(流式半截 JSON)时拿不到就
  // 退化为不可点 —— 宁可少一个入口,也不猜一个路径。
  const toolFilePath = useMemo(() => {
    if (node.name !== 'edit' && node.name !== 'write_file') return null
    const parsed = parseArgsObject(node.args)
    const raw = parsed?.['path']
    return typeof raw === 'string' && raw.trim() ? raw.trim() : null
  }, [node.name, node.args])
  const openFile = useFileOpen()
  const summary = running
    ? headSummary
    : node.result!.isError
      ? firstLine(node.result!.content)
      : headSummary || firstLine(node.result!.content)
  // 工具输出统一过一遍清洗:剥 ANSI 彩色/光标序列、归一 CRLF 与进度条回车
  // 覆盖。终端噪声在 <pre> 里可见即"乱码",修的是这个(见 stripAnsi 注释)。
  const outBody = useMemo(
    () => (node.result ? cleanToolOutput(node.result.content) : null),
    [node.result],
  )
  // bash:命令正文(剥掉后端注入的输出编码前缀)与切段后的输出。
  const commandBody = useMemo(
    () => (node.name === 'bash' ? bashCommand(node.args) : null),
    [node.name, node.args],
  )
  const bashOut = useMemo<BashOutput | null>(() => {
    if (node.name !== 'bash') return null
    // 结束后以结果的首尾预览为准;还在跑时用实时增量(见 fold 的 live)。
    if (outBody !== null) return bashOutputParts(outBody)
    return node.live ? liveBashOutput(node.live) : null
  }, [node.name, outBody, node.live])
  // 成功时卡片(diff / 清单 / bash 引用)已经把"改了什么"说全了,不再重复
  // out 那句结果文案;失败时保留 out(错误原因要看得见)。
  const showOut =
    node.result !== undefined && (node.result.isError || !(diff || todos || commandBody !== null))
  // 执行事实:优先读工具上报的 `meta`(退出码、结束原因),字段缺失时才
  // 从结果首行的"退出码: N"回退解析(旧日志与旧事件没有 `meta`)。回退
  // 规则写在 `bashExecution` 里,这里只管取。非 0 是数据不是工具失败,
  // 但要让人一眼看见 —— 挂在头部右侧。
  const exit = useMemo(
    () => (node.name === 'bash' ? bashExecution(node.result) : EMPTY_BASH_EXECUTION),
    [node.name, node.result],
  )
  const inputBody = useMemo(
    () => toolCallInput(node.name, node.args),
    [node.name, node.args],
  )
  // Hook 常驻组件顶层:条件 JSX 内挂 hook 会在展开/收起时改变 hook 数量。
  const labels = useMemo<MarkdownLabels>(
    () => ({
      code: { copyLabel: t('copy'), copiedLabel: t('copied') },
      footnotes: t('footnotes'),
    }),
    [localeRevision()],
  )
  const skillBody = useMemo(
    () => (node.result ? skillLoadBody(node.name, node.result.content, node.result.isError) : null),
    [node.name, node.result],
  )
  // ask 工具:问答卡片直接展示(不折叠),用户作答后转只读对照。
  // 等待期卡片必须常驻可见——折叠行会让用户错过提问。
  if (node.ask) {
    return (
      <div className="ask-row">
        <div className="ask-row-head">
          <span className="glyph"><IconToolAsk size={14} /></span>
          <span className="title">{t('askTitle')}</span>
        </div>
        <AskCard
          requestId={node.ask.requestId}
          questions={node.ask.questions}
          timeoutMs={node.ask.timeoutMs}
          startedAt={node.ask.startedAt}
          resolution={node.ask.resolution}
          onSubmit={(answers) => onAskAnswer?.(node.ask!.requestId, answers)}
          onCancel={() => onAskCancel?.(node.ask!.requestId)}
        />
      </div>
    )
  }
  return (
    <div
      className={`disc-row tool-row${open ? ' open' : ''}${running ? ' running' : ''}`}
    >
      <button className="disc-head" onClick={() => setOpen(!open)}>
        <span className="glyph">
          {running ? (
            <span className="dot run" />
          ) : node.result!.isError ? (
            <span className="dot err" />
          ) : (
            meta.icon
          )}
        </span>
        <span className="title">{meta.title}</span>
        <span className="sep" />
        {toolFilePath ? (
          /* edit / write_file 的文件路径：可点开在「文件读取」标签页。
             嵌在 disc-head 这个 button 里的是 span + role=button（不是嵌套
             button —— 那是非法 HTML），点击时 stopPropagation 拦住外层
             的展开/收起。 */
          <span
            className="summary tool-file-path"
            role="button"
            tabIndex={0}
            title={t('openInFileReader')}
            data-tool-file-path={toolFilePath}
            onClick={(event) => {
              event.stopPropagation()
              openFile(toolFilePath)
            }}
            onKeyDown={(event) => {
              if (event.key !== 'Enter' && event.key !== ' ') return
              event.preventDefault()
              event.stopPropagation()
              openFile(toolFilePath)
            }}
          >
            {summary}
          </span>
        ) : (
          <span
            className="summary"
            style={node.result?.isError ? { color: 'var(--error)' } : undefined}
          >
            {summary}
          </span>
        )}
        {diff && !node.result?.isError && (
          <DiffStat
            compact
            added={diff.diffs.reduce((sum, d) => sum + d.added, 0)}
            removed={diff.diffs.reduce((sum, d) => sum + d.removed, 0)}
          />
        )}
        <ExitBadge code={exit.code} reason={exit.reason} />
      </button>
      {open && (
        <div className="disc-body">
          {todos && <TodoCard snapshot={todos} />}
          {diff ? (
            diff.diffs.length === 1 ? (
              <DiffCard diff={diff.diffs[0]} />
            ) : (
              <div className="diff-group">
                {diff.diffs.map((item, index) => (
                  <DiffCard key={index} diff={item} hidePath />
                ))}
              </div>
            )
          ) : commandBody !== null ? (
            // bash:命令 + 输出走"引用"形态(无底色卡片,靠竖线分组)。
            <BashCard
              command={commandBody}
              output={bashOut ?? EMPTY_BASH_OUTPUT}
              exitCode={exit.code}
              failed={bashFailed(exit)}
              running={running}
            />
          ) : (
            !todos &&
            inputBody && (
              <div className="code-card">
                <div className="banner">in</div>
                <pre>{inputBody}</pre>
              </div>
            )
          )}
          {/* bash 已经由上面的 BashCard 完整呈现(命令 + stdout/stderr),
              不再挂通用 out 块;失败原因在结果里,BashCard 的红色竖线已表达。 */}
          {showOut && (
            <div className="code-card">
              <div className="banner">
                out
                {node.result!.isError ? (
                  <span style={{ color: 'var(--error)' }}>error</span>
                ) : (
                  <span style={{ color: 'var(--success)' }}>ok</span>
                )}
              </div>
              {skillBody ? (
                <div className="prose tool-skill-body">
                  <MarkdownText text={skillBody} streaming={false} labels={labels} />
                </div>
              ) : (
                <pre className={node.result!.isError ? 'err' : ''}>{outBody}</pre>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  )
}
