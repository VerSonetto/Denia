import type {
  AskQuestion,
  AskResolution,
  ContentBlock,
  SessionEnvelope,
  StreamChunk,
  TokenUsage,
  TurnEndReason,
  UserMessageImage,
} from './types'
import type { BlockType } from './generated/core'
import { editDiff, editStartLines, parseArgsObject, toolResultMeta, writeDiff } from './toolDisplay'
import type { EditDiff, TodoSnapshotItem, ToolResultMeta } from './toolDisplay'

/** `ask` 工具的问答载荷(挂在对应工具行上)。 */
export interface AskCardData {
  requestId: string
  questions: AskQuestion[]
  timeoutMs: number
  /** 请求发起时刻(epoch ms),倒计时用。 */
  startedAt: number
  /** 已结算时的结果;缺省 = 仍在等待。 */
  resolution?: AskResolution
}

/** One rendered block inside an assistant message. */
export interface UiBlock {
  kind: 'text' | 'reasoning' | 'tool-call'
  text: string
  id?: string
  name?: string
  args?: string
}

export type TranscriptNode =
  | { kind: 'user'; text: string; anchor?: number; images?: UserMessageImage[] }
  | {
      kind: 'context-injection'
      text: string
      seq?: number
      /**
       * 投递来源（`agent:<sender>` / `subagent-settled` / `job-completed` / …）。
       * 只有 agent-delivery 才有；其它注入通道缺省。UI 据此判方向。
       */
      source?: string
      /**
       * 本条由**本会话的父代理**投递（`agent:<header.parent_session>`）。
       * 与「子代理/其它代理 → 本会话」相反：前者是对话（用户气泡），
       * 后者是系统反馈（折叠行）。消息文本一字不改，只是 UI 投影不同。
       */
      fromParentAgent?: boolean
    }
  | { kind: 'system-prompt'; text: string }
  | {
      kind: 'assistant'
      turn: number
      step: number
      /** assistant-message 事件的 seq;流式未 settle 时缺省。分支锚点。 */
      seq?: number
      blocks: UiBlock[]
      usage?: TokenUsage
      interrupted: boolean
      streaming: boolean
      /** step-start 事件的 epoch ms;无 step-start 时为 undefined。 */
      stepStartTime?: number
      /**
       * 首个 token 帧(正文/推理/工具参数增量)的 epoch ms;TTFT =
       * firstTokenTime - stepStartTime。实时流从 chunk 推断,历史回放读
       * assistant-message 的 first_token_time(框架帧不算 token,旧日志两者皆无)。
       */
      firstTokenTime?: number
      /** assistant-message settle 的 epoch ms;decodeMs = settleTime - firstTokenTime。 */
      settleTime?: number
    }
  | {
      kind: 'tool'
      callId: string
      name: string
      args: string
      /** tool-call 事件的 seq;孤儿结果行用 result 事件的 seq。 */
      seq?: number
      result?: {
        content: string
        isError: boolean
        /**
         * 工具上报的结构化载荷(退出码、结束原因、产物引用……);旧日志与
         * 已落盘的旧事件没有这个键,渲染侧据此回退到展示文本。
         */
        meta?: ToolResultMeta
      }
      /**
       * 执行期间的实时输出增量(仅运行中的 bash 有,按流累积)。
       *
       * 只服务"命令还在跑"的观感:`tool-result` 一到就丢弃 —— 成果正文是
       * 首尾预览 + 完整产物,与增量流口径不同,留着只会多占一份可能上百
       * KB 的字符串。
       */
      live?: { stdout: string; stderr: string }
      /** `ask` 工具的提问载荷:挂在对应工具行上渲染问答卡片。 */
      ask?: AskCardData
      /**
       * todo_write 的前一次清单快照(上一次 todo-write 事件的 todos)。
       * 卡片据此标出"本次哪些条目推进了";首张清单为 undefined。
       */
      prevTodos?: TodoSnapshotItem[]
    }
  | { kind: 'turn-start'; turn: number; time: number }
  | {
      kind: 'turn-end'
      turn: number
      time: number
      reason: TurnEndReason
      usage?: TokenUsage
      /**
       * 本轮成功落盘的文件产物:write_file/edit 调用且工具结果非 error 的
       * 文件,按首次出现去重。仅 groupTranscript 在分组时填充,事件 fold
       * 不产此字段。
       */
      produced?: TurnProducedFile[]
    }
  | {
      kind: 'compaction'
      turn: number
      step: number
      summary: string
      replacesFrom: number
      replacesTo: number
      keepFrom: number
      preTokens?: number
      postTokens?: number
      seq: number
    }
  | {
      /**
       * 压缩进行中的占位行:**不来自事件流**,由前端在发起手动压缩时插入、
       * 收到结果后移除(真正的摘要会作为 compaction 事件落盘到达)。
       *
       * 后端 `compact_manually` 是一次 await、没有中间进度事件,所以这里
       * 只能表达"进行中",做不出百分比 —— 不假装有进度。
       */
      kind: 'compacting'
      /** 发起时刻(epoch ms)。 */
      startedAt: number
    }
  | {
      /** 本地斜杠命令回显(如 /goal):右对齐命令气泡,按事件 seq 排序。 */
      kind: 'command-echo'
      name: string
      text: string
      seq: number
    }

/**
 * 把一步的用量并入轮次总量。
 *
 * 冷启动与增量两条折叠路径**必须共用这一个函数**:先前各写一份累加,
 * 增量那份漏了 cacheRead/reasoning,同一轮对话"实时跑完"与"刷新后"
 * 会读出两组不同的数。input/output 恒存在,cache/推理无数据时不出现
 * (0 与"提供方没报"是两回事,不拿 0 冒充)。
 */
function mergeUsage(total: TokenUsage | null, usage?: TokenUsage): TokenUsage | null {
  if (!usage) return total
  const cacheRead = (total?.cacheReadTokens ?? 0) + (usage.cacheReadTokens ?? 0)
  const reasoning = (total?.reasoningTokens ?? 0) + (usage.reasoningTokens ?? 0)
  return {
    inputTokens: (total?.inputTokens ?? 0) + usage.inputTokens,
    outputTokens: (total?.outputTokens ?? 0) + usage.outputTokens,
    cacheReadTokens: cacheRead > 0 ? cacheRead : undefined,
    reasoningTokens: reasoning > 0 ? reasoning : undefined,
  }
}

/**
 * 后端内容块 → UI 块。
 *
 * **必须有兜底分支**:这里的输入是反序列化出来的外部数据,后端 `ContentBlock`
 * 加一种新 `"type"`(或某个网关塞进畸形块)时,没有 default 的 switch 会
 * **返回 undefined** —— 而调用方(`event.blocks.map(toUiBlock)`、`block-end`)
 * 会把它当成合法 `UiBlock` 塞进数组。渲染期读 `block.kind` 于是抛
 * `Cannot read properties of undefined (reading 'kind')`。
 *
 * 兜底按 `text` 处理:未知类型至少把能拿到的 `text`/`arguments` 显示出来,
 * 而不是让整条消息乃至整块 transcript 降级。
 */
function toUiBlock(block: ContentBlock): UiBlock {
  switch (block.type) {
    case 'text':
      return { kind: 'text', text: block.text }
    case 'reasoning':
      return { kind: 'reasoning', text: block.text }
    case 'tool-call':
      return {
        kind: 'tool-call',
        text: '',
        id: block.id,
        name: block.name,
        args: block.arguments,
      }
    default: {
      // TS 在这里把 block 收窄成 never(联合已穷尽),但**运行时未必**:数据
      // 来自反序列化。走 unknown 拿到真实形状,别信类型。
      const loose = block as unknown as Partial<{ type: string; text: string; arguments: string }> | null
      const text = typeof loose?.text === 'string'
        ? loose.text
        : typeof loose?.arguments === 'string'
          ? loose.arguments
          : ''
      return { kind: 'text', text }
    }
  }
}

/**
 * `block-start` 的块类型 → UI 块 kind。同 `toUiBlock`,不认识的类型按 `text`
 * 收:开一个能显示的空块,好过把一个 `undefined` kind 挂进渲染路径。
 */
function blockKindOf(blockType: BlockType): UiBlock['kind'] {
  return blockType === 'reasoning' || blockType === 'tool-call' ? blockType : 'text'
}

/**
 * 把 chunk 声称的下标归一成**可以安全写入**的下标;返回 -1 表示这帧该丢。
 *
 * 为什么必须有:流式帧的 `index` 是提供方给的,不是我们数出来的。三条真实
 * 路径都会让它超过已开块数——
 * 1. 非规范网关:缺 index 的 tool-call 帧由后端"尽力归位"推算
 *    (`crates/llm/src/wire.rs` 的 `resolve_wire_index`);
 * 2. 网络错误重试:模型从头重新生成,重发低位 index 之后又续上高位;
 * 3. 前端节点被重建:`assistant-chunk` 是 transient 事件,不进快照也不进
 *    分页窗口(`store.rs::read_page` 按 `is_transient_event` 过滤),断流
 *    重连后补发的增量落在一个 `blocks: []` 的新节点上,index 却仍是本次
 *    attempt 内的高位值。
 *
 * 后果不是"显示不全"而是**整块 transcript 崩掉**:`next[index] = ...` 越界写
 * 造出稀疏空洞,而空洞只是暂时不抛(`map`/`some` 会跳过没有的索引);下一次
 * 任意一次 `[...blocks]` 展开会把空洞复制成**致密的 undefined**,于是渲染期
 * `blocks.some((b) => b.kind …)` 抛
 * `Cannot read properties of undefined (reading 'kind')`。同一份数据"实时跑
 * 完不崩、刷新才崩"或反之,取决于中间有没有那一次展开 —— 这正是它表现为
 * 偶发的原因。
 *
 * 归一规则:超过块数就折叠到末尾追加(语义上就是"又开了一个新块");下标
 * 根本不是非负整数(畸形帧)则丢弃该帧 —— 宁可少一段正文,也不能把正文串
 * 进别的块里冒充它。
 */
function slotFor(index: number, length: number): number {
  if (typeof index !== 'number' || !Number.isInteger(index) || index < 0) return -1
  return index > length ? length : index
}

/**
 * 在归一后的下标上写入。越界折叠为追加,畸形下标原样返回(不产空洞)。
 *
 * 读也走同一个归一下标:否则"写到 5、读 3"会让增量串进错误的块里。
 */
function patchAt(
  blocks: UiBlock[],
  rawIndex: number,
  patch: (previous: UiBlock | undefined) => UiBlock,
): UiBlock[] {
  const index = slotFor(rawIndex, blocks.length)
  if (index < 0) return blocks
  const next = [...blocks]
  if (index === next.length) next.push(patch(undefined))
  else next[index] = patch(next[index])
  return next
}

/**
 * Applies one stream chunk to an immutable block list. The delta decides the
 * block kind when it arrives before its `block-start` (reasoning deltas are
 * never rendered as plain text); an existing loose `text` block is upgraded,
 * never downgraded. `block-end` replaces the block with the authority.
 *
 * 所有下标都先过 [`slotFor`]:提供方给的 index 不可信,越界写会造出渲染期
 * 读得到的 undefined(见那里的说明)。
 */
function applyChunk(blocks: UiBlock[], chunk: StreamChunk): UiBlock[] {
  switch (chunk.type) {
    case 'block-start': {
      // 正常流:delta 携带的 index 与已打开块数一致。异常流(网络错误重试,
      // 模型从头重新生成)会重发同 index 的 block-start,而后续 delta 仍按
      // 原 index 路由 —— 原样追加会造出一个永远收不到内容的空块(并且旧块
      // 还会串进重试后的 delta)。所以该位置已有块就跳过这次重复开启;越界的
      // index 经 `slotFor` 折叠到末尾,于是这里恒为追加,不再可能留下空洞。
      const index = slotFor(chunk.index, blocks.length)
      if (index < 0) return blocks
      if (index < blocks.length) return blocks
      return [...blocks, { kind: blockKindOf(chunk.block_type), text: '' }]
    }
    case 'text-delta':
    case 'reasoning-delta': {
      const kind = chunk.type === 'reasoning-delta' ? 'reasoning' as const : 'text' as const
      return patchAt(blocks, chunk.index, (block) => {
        const previous = block ?? { kind, text: '' }
        return {
          ...previous,
          kind: previous.kind === 'text' ? kind : previous.kind,
          text: previous.text + chunk.text,
        }
      })
    }
    case 'tool-call-delta': {
      return patchAt(blocks, chunk.index, (block) => {
        const previous = block ?? { kind: 'tool-call' as const, text: '' }
        return {
          ...previous,
          args: (previous.args ?? '') + chunk.arguments_delta,
          id: chunk.id || previous.id,
          name: chunk.name ?? previous.name,
        }
      })
    }
    case 'block-end': {
      const index = slotFor(chunk.index, blocks.length)
      // index 越界 = 我们没开过这个块,没有可结算的对象。这里**丢帧而不是
      // 追加**:block-end 的正文与已到达的 delta 同源,追加会把同一段文字
      // 显示两遍;真正的权威是随后落盘的 assistant-message(它整体重建 blocks)。
      if (index < 0 || index >= blocks.length) return blocks
      const settled = toUiBlock(chunk.block)
      return patchAt(blocks, chunk.index, (block) => ({
        ...settled,
        text: settled.text || block?.text || '',
      }))
    }
    default:
      return blocks
  }
}

/**
 * 该 chunk 是否承载模型的一个输出 token:非空正文/推理增量,或工具调用
 * 参数增量(带名字的首帧也算)。block-start/block-end/usage/finish 是
 * 流框架帧,不算 —— 首 token 延迟锚定第一个真正的输出增量。
 */
function isTokenDeltaChunk(chunk: StreamChunk): boolean {
  switch (chunk.type) {
    case 'text-delta':
    case 'reasoning-delta':
      return chunk.text !== ''
    case 'tool-call-delta':
      return chunk.arguments_delta !== '' || chunk.name !== undefined
    default:
      return false
  }
}

/**
 * 实时输出增量该并进哪条流:只认 `stderr` 这一条旁路,其余(含未来可能
 * 新增的流名)一律并入 stdout —— 界面上只有这两处能显示,认不出的流名
 * 丢掉会让输出凭空消失,归并至少保证"看得见"。
 */
function liveKey(stream: string): 'stdout' | 'stderr' {
  return stream === 'stderr' ? 'stderr' : 'stdout'
}

/** 增量累积(copy-on-write 版,增量 fold 路径用)。 */
function withLive(
  node: Extract<TranscriptNode, { kind: 'tool' }>,
  stream: string,
  text: string,
): Extract<TranscriptNode, { kind: 'tool' }> {
  const live = node.live ?? { stdout: '', stderr: '' }
  const key = liveKey(stream)
  return { ...node, live: { ...live, [key]: live[key] + text } }
}

/** 增量累积(原地版,冷路径用:那里的节点本来就是新造的)。 */
function appendLive(
  node: Extract<TranscriptNode, { kind: 'tool' }>,
  stream: string,
  text: string,
): void {
  const live = (node.live ??= { stdout: '', stderr: '' })
  const key = liveKey(stream)
  live[key] += text
}

/**
 * agent-delivery 的 UI 投影:来源 + 是否来自父代理。
 *
 * 方向判据只有一条:**投递来源与本会话父 id 比对**。`source` 是后端写死的
 * 稳定事实(`agent:<发信方会话 id>` / `subagent-settled` / `job-completed`),
 * 且在 `SessionEvent::AgentDelivery` 里是非可选 `String`(没有 default,
 * 缺字段整条日志都反序列化不了),所以不存在"没有 source"的历史数据,
 * 也就没有按中文正文猜方向的第二条路径 —— 正文里的 `[父代理 … 委派任务]`
 * 是给模型看的人话,换措辞就失效,拿它当判据迟早把子代理的完成通知画成对话。
 *
 * `parentSessionId` 来自会话头 `parentSession`;缺省(主会话 / 头缺失)
 * 一律按反馈处理 —— 宁可少画一个气泡,也不把系统反馈说成"用户在说话"。
 */
export function deliveryProjection(
  event: { text: string; source: string },
  parentSessionId?: string,
): Pick<Extract<TranscriptNode, { kind: 'context-injection' }>, 'source' | 'fromParentAgent'> {
  return {
    source: event.source,
    fromParentAgent: parentSessionId !== undefined && event.source === `agent:${parentSessionId}`,
  }
}

/**
 * The deterministic fold: identical output for cold history and live
 * streaming. Chunk deltas accumulate into an in-progress assistant node;
 * the settled `assistant-message` replaces it with authoritative blocks.
 *
 * `parentSessionId` 只影响 agent-delivery 节点的 UI 方向标记（见
 * [`deliveryProjection`]），不参与任何去重与折叠判定。
 */
export function foldEvents(events: SessionEnvelope[], parentSessionId?: string): TranscriptNode[] {
  const nodes: TranscriptNode[] = []
  let open: Extract<TranscriptNode, { kind: 'assistant' }> | null = null
  const tools = new Map<string, Extract<TranscriptNode, { kind: 'tool' }>>()
  let turnUsage: TokenUsage | null = null
  // step-start 时间表:turn:step → epoch ms,供 assistant 节点算 TTFT。
  const stepStarts = new Map<string, number>()
  // 当前清单快照:供下一次 todo_write 卡片比对"这次推进了哪条"。
  let lastTodos: TodoSnapshotItem[] | undefined

  const addUsage = (usage?: TokenUsage) => {
    turnUsage = mergeUsage(turnUsage, usage)
  }

  const closeOpen = () => {
    if (open) {
      open.streaming = false
      open = null
    }
  }

  // 上一次已显示的 system-prompt 全文(内容去重依据,见下方 case)。
  let lastSystemPrompt: string | null = null

  for (const event of events) {
    switch (event.type) {
      case 'agent-delivery':
        closeOpen()
        nodes.push({
          kind: 'context-injection',
          text: event.text,
          seq: event.seq,
          ...deliveryProjection(event, parentSessionId),
        })
        break
      case 'tool-output-chunk': {
        // 实时输出增量:挂到对应工具行上(命令还在跑才有;结果一到作废,
        // 见 tool 节点的 live 注释)。找不到行的(日志被截断)就丢掉 ——
        // 它只是观感,不值得为它造一个游离节点。
        const node = tools.get(event.call_id)
        // 已出结果的调用不再回填:读侧被中止时增量可能迟到于 tool-result,
        // 回填会把那份可能上百 KB 的字符串永远挂在节点上。
        if (node && !node.result) appendLive(node, event.stream, event.text)
        break
      }
      case 'command-run':
        closeOpen()
        nodes.push({ kind: 'command-echo', name: event.name, text: event.text, seq: event.seq })
        break
      case 'turn-start':
        closeOpen()
        nodes.push({ kind: 'turn-start', turn: event.turn, time: event.time })
        break
      case 'user-message':
        closeOpen()
        if (event.injected) {
          nodes.push({ kind: 'context-injection', text: event.text, seq: event.seq })
        } else {
          nodes.push({
            kind: 'user',
            text: event.text,
            anchor: event.seq,
            images: event.images?.length ? event.images : undefined,
          })
        }
        break
      case 'system-prompt':
        // 后端按 step 判重,而 step 是本 turn 内计数,每轮第一个 step 恒为
        // 1 —— 于是内容一字未变的 system-prompt 每轮都记一条。这里按内容
        // 去重:只在与上一次不同时才显示(切模型 / 改系统提示词 / 换 persona
        // 都会让文本变化,那些必须显示)。
        closeOpen()
        if (lastSystemPrompt !== event.text) {
          lastSystemPrompt = event.text
          nodes.push({ kind: 'system-prompt', text: event.text })
        }
        break
      case 'step-start':
        stepStarts.set(`${event.turn}:${event.step}`, event.time)
        break
      case 'step-end':
        break
      case 'agent-preset':
        // 会话事实(会话运行哪份组装),不产生转录节点:当前值由会话流的
        // 订阅方单独取用,这里只保证 fold 不把它当未知事件截断后续。
        break
      case 'assistant-chunk': {
        if (!open || open.turn !== event.turn || open.step !== event.step) {
          closeOpen()
          open = {
            kind: 'assistant',
            turn: event.turn,
            step: event.step,
            blocks: [],
            interrupted: false,
            streaming: true,
            stepStartTime: stepStarts.get(`${event.turn}:${event.step}`),
          }
          nodes.push(open)
        }
        if (open.firstTokenTime === undefined && isTokenDeltaChunk(event.chunk)) {
          open.firstTokenTime = event.time
        }
        open.blocks = applyChunk(open.blocks, event.chunk)
        break
      }
      case 'assistant-message': {
        const blocks = event.blocks.map(toUiBlock)
        if (open && open.turn === event.turn && open.step === event.step) {
          open.blocks = blocks
          open.usage = event.usage
          open.interrupted = event.interrupted ?? false
          open.settleTime = event.time
          open.seq = event.seq
          // 后端落盘的首 token 时间是权威事实;旧日志无此字段时回退
          // chunk 推断值。
          if (event.first_token_time !== undefined) open.firstTokenTime = event.first_token_time
          closeOpen()
        } else {
          closeOpen()
          nodes.push({
            kind: 'assistant',
            turn: event.turn,
            step: event.step,
            seq: event.seq,
            blocks,
            usage: event.usage,
            interrupted: event.interrupted ?? false,
            streaming: false,
            stepStartTime: stepStarts.get(`${event.turn}:${event.step}`),
            firstTokenTime: event.first_token_time,
            settleTime: event.time,
          })
        }
        addUsage(event.usage)
        break
      }
      case 'tool-call': {
        closeOpen()
        const node: Extract<TranscriptNode, { kind: 'tool' }> = {
          kind: 'tool',
          callId: event.call_id,
          name: event.name,
          args: event.arguments,
          seq: event.seq,
        }
        // todo_write:带上本次调用之前的清单快照(工具执行时才 emit
        // todo-write 事件,所以这里的 lastTodos 就是"上一次的")。
        if (event.name === 'todo_write') node.prevTodos = lastTodos
        tools.set(event.call_id, node)
        nodes.push(node)
        break
      }
      case 'todo-write':
        // 清单快照(last-write-wins):只更新状态,不产生对话流节点 ——
        // 清单由对应的 todo_write 工具行渲染,避免同一份数据出现两处。
        lastTodos = event.todos
        break
      case 'ask-requested': {
        const node = tools.get(event.call_id)
        const ask: AskCardData = {
          requestId: event.request_id,
          questions: event.questions,
          timeoutMs: event.timeout_ms,
          startedAt: event.time,
        }
        if (node) {
          node.ask = ask
        } else {
          closeOpen()
          nodes.push({
            kind: 'tool',
            callId: event.call_id,
            name: 'ask',
            args: '',
            seq: event.seq,
            ask,
          })
        }
        break
      }
      case 'ask-resolved': {
        for (const node of nodes) {
          if (node.kind === 'tool' && node.ask?.requestId === event.request_id) {
            node.ask = { ...node.ask, resolution: event.resolution }
            break
          }
        }
        break
      }
      case 'tool-result': {
        // 带 replaces 的 tool-result 是模型侧的原位替换(微压缩清理),
        // 对话流里继续显示原始调用/结果,不落占位行。
        if (event.replaces != null) break
        const node = tools.get(event.call_id)
        const result = {
          content: event.content,
          isError: event.is_error,
          meta: toolResultMeta(event.meta),
        }
        if (node) {
          node.result = result
          delete node.live
        } else {
          nodes.push({
            kind: 'tool',
            callId: event.call_id,
            name: '?',
            args: '',
            seq: event.seq,
            result,
          })
        }
        break
      }
      case 'compaction-summary': {
        closeOpen()
        nodes.push({
          kind: 'compaction',
          turn: event.turn,
          step: event.step,
          summary: event.summary,
          replacesFrom: event.replaces_from,
          replacesTo: event.replaces_to,
          keepFrom: event.keep_from,
          preTokens: event.pre_tokens,
          postTokens: event.post_tokens,
          seq: event.seq,
        })
        break
      }
      case 'turn-end':
        closeOpen()
        nodes.push({
          kind: 'turn-end',
          turn: event.turn,
          time: event.time,
          reason: event.reason,
          usage: turnUsage ?? undefined,
        })
        turnUsage = null
        break
      default:
        break
    }
  }
  closeOpen()
  // 冷启动(切会话/重拉快照)是全量权威结果:把增量路径的暂存对齐到它,
  // 否则换会话后模块级暂存还留着上一个会话的值,新会话首条 system-prompt
  // 可能因与旧会话文本相同而被误吞,todo 也会比对出假变化。
  incrementalLastTodos = lastTodos
  incrementalLastSystemPrompt = lastSystemPrompt
  incrementalParentSessionId = parentSessionId
  incrementalStepStarts.clear()
  for (const [key, time] of stepStarts) incrementalStepStarts.set(key, time)
  return nodes
}

/** A turn is live when some `turn-start` lacks its `turn-end`. */
export function hasOpenTurn(events: SessionEnvelope[]): boolean {
  let open = false
  for (const event of events) {
    if (event.type === 'turn-start') open = true
    else if (event.type === 'turn-end') open = false
  }
  return open
}

/** 当前未闭合 turn 的起点(epoch ms);无 open turn 返回 undefined。 */
export function openTurnStartedAt(nodes: TranscriptNode[]): number | undefined {
  let started: number | undefined
  for (const node of nodes) {
    if (node.kind === 'turn-start') started = node.time
    else if (node.kind === 'turn-end') started = undefined
  }
  return started
}

/** 增量 fold 的 step-start 时间暂存:turn:step → epoch ms。 */
const incrementalStepStarts = new Map<string, number>()

/** 增量 fold 的清单快照暂存:供下一次 todo_write 卡片比对变化。 */
let incrementalLastTodos: TodoSnapshotItem[] | undefined

/** 增量 fold 的 system-prompt 暂存:与冷启动路径同规则的内容去重依据。 */
let incrementalLastSystemPrompt: string | null = null

/**
 * 增量 fold 的父会话 id 暂存:agent-delivery 方向判据的另一半(见
 * [`deliveryProjection`])。同样由冷启动对齐 —— 换会话后必须跟着新头走,
 * 否则旧会话的 id 会把新会话里的 `agent:*` 投递误判成父代理来信。
 */
let incrementalParentSessionId: string | undefined

/**
 * Incremental fold: applies one envelope to an existing node list without
 * refolding the prefix. Only the affected tail node is copied, so a live
 * stream costs O(1) per envelope instead of O(n).
 */
export function applyEnvelope(
  nodes: TranscriptNode[],
  event: SessionEnvelope,
): TranscriptNode[] {
  return applyEnvelopes(nodes, [event])
}

/**
 * 批量应用一帧内的多条事件(流式 rAF 合并路径)。
 *
 * 单条逐次 `applyEnvelope` 对连续 assistant-chunk 是 O(m·n):每条都要
 * `[...nodes.slice(0, -1), { ...last, ... }]` 复制整个节点数组。流式高频
 * 帧(一帧可达数十条 chunk)在长会话上会放大成每帧 O(m·n) 的数组复制。
 *
 * 这里把落在同一流式 assistant 节点上的连续 chunk 累积进一个可变
 * `blocks` 缓冲,中途完全不动节点数组;遇到非 chunk 事件(settle/
 * turn-end/工具调用等)或帧末,才把缓冲一次 flush 成新数组。连续 chunk
 * 的数组复制从每帧 m 次降到 1 次,语义与 `applyEnvelope` 逐条应用完全一致。
 */
export function applyEnvelopes(
  nodes: TranscriptNode[],
  events: SessionEnvelope[],
): TranscriptNode[] {
  if (events.length === 0) return nodes
  let current = nodes
  // 正在累积的流式尾节点:存在时 current 尾部就是它(引用未变,
  // 只有 accumulate 真实发生时才在 flush 时重建数组)。
  interface Accum {
    node: Extract<TranscriptNode, { kind: 'assistant' }>
    blocks: UiBlock[]
  }
  let acc: Accum | null = null
  const flush = (): Extract<TranscriptNode, { kind: 'assistant' }> | null => {
    if (acc === null) return null
    const settled = { ...acc.node, blocks: acc.blocks }
    current = current.slice(0, -1)
    current.push(settled)
    acc = null
    return settled
  }
  for (const event of events) {
    if (event.type === 'assistant-chunk') {
      const last = current[current.length - 1]
      if (
        acc !== null &&
        last?.kind === 'assistant' &&
        last.streaming &&
        last.turn === event.turn &&
        last.step === event.step
      ) {
        if (acc.node.firstTokenTime === undefined && isTokenDeltaChunk(event.chunk)) {
          acc.node = { ...acc.node, firstTokenTime: event.time }
        }
        acc.blocks = applyChunk(acc.blocks, event.chunk)
        continue
      }
      // 先落掉上一个累积节点(节点迁移),再开新累积。
      if (acc !== null) flush()
      const tail = current[current.length - 1]
      if (
        tail?.kind === 'assistant' &&
        tail.streaming &&
        tail.turn === event.turn &&
        tail.step === event.step
      ) {
        const node = isTokenDeltaChunk(event.chunk) ? { ...tail, firstTokenTime: tail.firstTokenTime ?? event.time } : tail
        const blocks = applyChunk(node.blocks, event.chunk)
        acc = { node, blocks }
      } else {
        const fresh: Extract<TranscriptNode, { kind: 'assistant' }> = {
          kind: 'assistant',
          turn: event.turn,
          step: event.step,
          blocks: [],
          interrupted: false,
          streaming: true,
          stepStartTime: incrementalStepStarts.get(`${event.turn}:${event.step}`),
        }
        if (isTokenDeltaChunk(event.chunk)) fresh.firstTokenTime = event.time
        acc = { node: fresh, blocks: applyChunk(fresh.blocks, event.chunk) }
        current = [...current, fresh]
      }
      continue
    }
    if (acc !== null) flush()
    current = applyEnvelopeStep(current, event)
  }
  if (acc !== null) flush()
  return current
}

/** 单条事件应用(applyEnvelope 的主体,供批量路径逐条复用)。 */
function applyEnvelopeStep(
  nodes: TranscriptNode[],
  event: SessionEnvelope,
): TranscriptNode[] {
  switch (event.type) {
    case 'agent-delivery':
      return [
        ...nodes,
        {
          kind: 'context-injection',
          text: event.text,
          seq: event.seq,
          ...deliveryProjection(event, incrementalParentSessionId),
        },
      ]
    case 'command-run':
      return [
        ...nodes,
        { kind: 'command-echo', name: event.name, text: event.text, seq: event.seq },
      ]
    case 'turn-start':
      incrementalStepStarts.clear()
      return [...nodes, { kind: 'turn-start', turn: event.turn, time: event.time }]
    case 'user-message':
      if (event.injected) {
        return [...nodes, { kind: 'context-injection', text: event.text, seq: event.seq }]
      }
      return [
        ...nodes,
        {
          kind: 'user',
          text: event.text,
          anchor: event.seq,
          images: event.images?.length ? event.images : undefined,
        },
      ]
    case 'system-prompt':
      // 与冷启动路径同规则:内容未变则不新增节点(后端按 step 判重,而
      // step 是本 turn 内计数,每轮恒为 1,导致未变化的提示词每轮都记)。
      if (incrementalLastSystemPrompt === event.text) return nodes
      incrementalLastSystemPrompt = event.text
      return [...nodes, { kind: 'system-prompt', text: event.text }]
    case 'step-start':
      incrementalStepStarts.set(`${event.turn}:${event.step}`, event.time)
      return nodes
    case 'step-end':
      return nodes
    case 'agent-preset':
      return nodes
    case 'assistant-chunk': {
      const last = nodes[nodes.length - 1]
      if (
        last?.kind === 'assistant' &&
        last.streaming &&
        last.turn === event.turn &&
        last.step === event.step
      ) {
        const node =
          last.firstTokenTime === undefined && isTokenDeltaChunk(event.chunk)
            ? { ...last, firstTokenTime: event.time }
            : last
        return [...nodes.slice(0, -1), { ...node, blocks: applyChunk(node.blocks, event.chunk) }]
      }
      const fresh: Extract<TranscriptNode, { kind: 'assistant' }> = {
        kind: 'assistant',
        turn: event.turn,
        step: event.step,
        blocks: [],
        interrupted: false,
        streaming: true,
        stepStartTime: incrementalStepStarts.get(`${event.turn}:${event.step}`),
      }
      if (isTokenDeltaChunk(event.chunk)) fresh.firstTokenTime = event.time
      return [...nodes, { ...fresh, blocks: applyChunk(fresh.blocks, event.chunk) }]
    }
    case 'assistant-message': {
      const last = nodes[nodes.length - 1]
      // settle 时保留流式阶段积累的时间戳;后端落盘的首 token 时间优先。
      const prior = last?.kind === 'assistant' && last.turn === event.turn && last.step === event.step
        ? last
        : undefined
      const settled: TranscriptNode = {
        kind: 'assistant',
        turn: event.turn,
        step: event.step,
        seq: event.seq,
        blocks: event.blocks.map(toUiBlock),
        usage: event.usage,
        interrupted: event.interrupted ?? false,
        streaming: false,
        stepStartTime: prior?.stepStartTime ?? incrementalStepStarts.get(`${event.turn}:${event.step}`),
        firstTokenTime: event.first_token_time ?? prior?.firstTokenTime,
        settleTime: event.time,
      }
      if (
        last?.kind === 'assistant' &&
        last.streaming &&
        last.turn === event.turn &&
        last.step === event.step
      ) {
        return [...nodes.slice(0, -1), settled]
      }
      return [...nodes, settled]
    }
    case 'tool-call': {
      const base: Extract<TranscriptNode, { kind: 'tool' }> = {
        kind: 'tool',
        callId: event.call_id,
        name: event.name,
        args: event.arguments,
        seq: event.seq,
      }
      // 与冷路径一致:todo_write 带上本次调用之前的清单快照。
      if (event.name === 'todo_write') base.prevTodos = incrementalLastTodos
      return [...nodes, base]
    }
    case 'todo-write':
      // 清单快照(last-write-wins):不产生节点,清单由工具行渲染。
      incrementalLastTodos = event.todos
      return nodes
    // ask 的提问/结算挂到对应工具行(卡片与工具行同体,不产生游离节点)。
    case 'ask-requested': {
      const patch = (node: Extract<TranscriptNode, { kind: 'tool' }>) => ({
        ...node,
        ask: {
          requestId: event.request_id,
          questions: event.questions,
          timeoutMs: event.timeout_ms,
          startedAt: event.time,
          resolution: undefined,
        },
      })
      for (let i = nodes.length - 1; i >= 0; i--) {
        const node = nodes[i]
        if (node.kind === 'tool' && node.callId === event.call_id) {
          const copy = nodes.slice()
          copy[i] = patch(node)
          return copy
        }
      }
      // 找不到对应工具行(日志截断等):以孤儿工具行承载卡片。
      return [
        ...nodes,
        {
          kind: 'tool',
          callId: event.call_id,
          name: 'ask',
          args: '',
          seq: event.seq,
          ask: {
            requestId: event.request_id,
            questions: event.questions,
            timeoutMs: event.timeout_ms,
            startedAt: event.time,
          },
        },
      ]
    }
    case 'ask-resolved': {
      for (let i = nodes.length - 1; i >= 0; i--) {
        const node = nodes[i]
        if (node.kind === 'tool' && node.ask?.requestId === event.request_id) {
          const copy = nodes.slice()
          copy[i] = { ...node, ask: { ...node.ask, resolution: event.resolution } }
          return copy
        }
      }
      return nodes
    }
    case 'compaction-summary':
      return [
        ...nodes,
        {
          kind: 'compaction',
          turn: event.turn,
          step: event.step,
          summary: event.summary,
          replacesFrom: event.replaces_from,
          replacesTo: event.replaces_to,
          keepFrom: event.keep_from,
          preTokens: event.pre_tokens,
          postTokens: event.post_tokens,
          seq: event.seq,
        },
      ]
    case 'tool-output-chunk': {
      for (let i = nodes.length - 1; i >= 0; i--) {
        const node = nodes[i]
        if (node.kind === 'tool' && node.callId === event.call_id) {
          // 已出结果的调用不再回填(读侧被中止时增量可能迟到于结果)。
          if (node.result) return nodes
          const copy = nodes.slice()
          copy[i] = withLive(node, event.stream, event.text)
          return copy
        }
      }
      return nodes
    }
    case 'tool-result': {
      // 带 replaces 的 tool-result 是模型侧的原位替换(微压缩清理),
      // 对话流里继续显示原始调用/结果,不落占位行。
      if (event.replaces != null) return nodes
      for (let i = nodes.length - 1; i >= 0; i--) {
        const node = nodes[i]
        if (node.kind === 'tool' && node.callId === event.call_id) {
          const copy = nodes.slice()
          copy[i] = {
            ...node,
            result: {
              content: event.content,
              isError: event.is_error,
              meta: toolResultMeta(event.meta),
            },
            // 实时增量到此作废(冷路径同规则)。
            live: undefined,
          }
          return copy
        }
      }
      return [
        ...nodes,
        {
          kind: 'tool',
          callId: event.call_id,
          name: '?',
          args: '',
          seq: event.seq,
          result: {
            content: event.content,
            isError: event.is_error,
            meta: toolResultMeta(event.meta),
          },
        },
      ]
    }
    case 'turn-end': {
      incrementalStepStarts.clear()
      // 与冷启动路径共用 mergeUsage:反向扫本轮 assistant 步,四字段全累加。
      // 先前这里只加 input/output,缓存读与推理实时恒为 0、刷新后才有值。
      let usage: TokenUsage | null = null
      for (let i = nodes.length - 1; i >= 0; i--) {
        const node = nodes[i]
        if (node.kind === 'turn-end') break
        if (node.kind === 'assistant') usage = mergeUsage(usage, node.usage)
      }
      return [
        ...nodes,
        {
          kind: 'turn-end',
          turn: event.turn,
          time: event.time,
          reason: event.reason,
          usage: usage ?? undefined,
        },
      ]
    }
    default:
      return nodes
  }
}

/* ---- presentation grouping ---- */

/** One closed turn's folded prefix: work duration, tool count, hidden rows. */
export interface OverviewRow {
  kind: 'overview'
  durationMs: number
  toolCount: number
  hidden: TranscriptNode[]
}

/**
 * 在对话流末尾挂上"正在压缩"占位行;已在压缩中则原样返回(幂等,避免
 * 连点产生两行)。
 */
export function withCompacting(nodes: TranscriptNode[], startedAt: number): TranscriptNode[] {
  if (nodes.some((node) => node.kind === 'compacting')) return nodes
  return [...nodes, { kind: 'compacting', startedAt }]
}

/** 移除压缩占位行:压缩结束(成功、无可压缩、失败)都走这里。 */
export function withoutCompacting(nodes: TranscriptNode[]): TranscriptNode[] {
  if (!nodes.some((node) => node.kind === 'compacting')) return nodes
  return nodes.filter((node) => node.kind !== 'compacting')
}

export type TranscriptRow =
  | { kind: 'node'; node: TranscriptNode }
  | OverviewRow

/* ---- 收尾轮次的文件产物 ---- */

/**
 * 收尾行展示的**一个**产物文件:本轮对它做过什么、改了多少、怎么改的。
 *
 * 三段信息是一张卡片的全部事实,缺一段都会让人回头去翻工具行:
 * - `kind` 决定标题说"新建"还是"编辑"(先建后改的文件按新建计);
 * - `added`/`removed` 是本轮累计行数,跨多次编辑累加;
 * - `diffs` 是本轮各次调用的行级 diff(已各自折叠到 40 行内),供悬停预览
 *   与单文件内联展示复用 —— 不用二次解析,也不用再拉一次 diff。
 */
export interface TurnProducedFile {
  /** 工具参数里的原始路径(相对 cwd 或绝对,原样保留)。 */
  path: string
  kind: 'write' | 'edit'
  added: number
  removed: number
  diffs: EditDiff[]
}

/**
 * 变更类工具调用参数里的目标文件路径;非变更调用、参数不完整或路径为空
 * 返回 null。只有 write_file 与 edit 是第一方文件变更工具。
 *
 * edit 的两种形式都认:单处(old_string/new_string 成对且不同)与
 * 多处(edits 数组每项都是合法且非 no-op 的编辑)。
 */
function mutationPath(name: string, args: string): string | null {
  if (name !== 'write_file' && name !== 'edit') return null
  const parsed = parseArgsObject(args)
  if (!parsed) return null
  const path = typeof parsed.path === 'string' && parsed.path.trim().length > 0 ? parsed.path : null
  if (path === null) return null
  if (name === 'write_file') return typeof parsed.content === 'string' ? path : null
  const edits = parsed.edits
  if (Array.isArray(edits)) {
    return edits.length > 0 &&
      edits.every((raw) => {
        if (raw === null || typeof raw !== 'object') return false
        const item = raw as Record<string, unknown>
        return (
          typeof item.old_string === 'string' &&
          item.old_string.length > 0 &&
          typeof item.new_string === 'string' &&
          item.old_string !== item.new_string &&
          (item.replace_all === undefined || typeof item.replace_all === 'boolean')
        )
      })
      ? path
      : null
  }
  const oldString = parsed.old_string
  return typeof oldString === 'string' &&
    oldString.length > 0 &&
    typeof parsed.new_string === 'string' &&
    oldString !== parsed.new_string &&
    (parsed.replace_all === undefined || typeof parsed.replace_all === 'boolean')
    ? path
    : null
}

type ToolNode = Extract<TranscriptNode, { kind: 'tool' }>
type TurnEndNode = Extract<TranscriptNode, { kind: 'turn-end' }>

/**
 * 单个工具节点的产物信息缓存。分组在每次渲染都会重跑,而 write_file 的
 * 参数可能很大,不能每帧 JSON.parse + diff;键是节点引用,结果到达
 * (result 引用变化)时重算一次。
 */
const producedPathCache = new WeakMap<
  ToolNode,
  { result: unknown; info: TurnProducedFile | null }
>()

/**
 * 单个成功变更调用的产物画像:path + 本次调用的增删行数 + 折叠后的 hunk。
 *
 * 行数口径:edit 是 old/new string 的行级 diff(`editDiff`);write_file 没有
 * 旧内容可对照,按"纯新增"计(content 行数)——与工具行展开的 diff 同一口径,
 * 两处数字不会打架。
 */
function toolProducedInfo(node: ToolNode): TurnProducedFile | null {
  const cached = producedPathCache.get(node)
  if (cached && cached.result === node.result) return cached.info
  let info: TurnProducedFile | null = null
  if (node.result && !node.result.isError) {
    const path = mutationPath(node.name, node.args)
    if (path !== null) {
      if (node.name === 'edit') {
        const diffs = editDiff(node.name, node.args, editStartLines(node.result.content))
        if (diffs) {
          let added = 0
          let removed = 0
          for (const diff of diffs.diffs) {
            added += diff.added
            removed += diff.removed
          }
          info = { path, kind: 'edit', added, removed, diffs: diffs.diffs }
        }
      } else {
        const diff = writeDiff(node.name, node.args)
        if (diff) {
          info = { path, kind: 'write', added: diff.added, removed: diff.removed, diffs: [diff] }
        }
      }
    }
  }
  producedPathCache.set(node, { result: node.result, info })
  return info
}

/** 每个产物文件最多保留的 hunk 帧数:预览浮层与收尾卡片都不该被一轮
 *  超长编辑(比如连改十几处的大重构)撑爆;超出部分只计入 ±统计。 */
const PRODUCED_DIFF_FRAMES = 8

/**
 * 给收尾轮次挂上本轮文件产物;无产物时原节点返回(引用稳定,行 memo 不
 * 失效)。按收尾节点身份在首次分组时冻结:工具结果先于 turn-end 落盘,
 * 之后才到的结果不在口径内。
 *
 * 同一文件被多次编辑时合并:±行数累计,hunk 按调用顺序追加(封顶
 * [`PRODUCED_DIFF_FRAMES`] 帧),操作类型记首次(write 优先于 edit ——
 * "先建后改"的文件在本轮语义上是新建)。
 */
const producedTurnCache = new WeakMap<TurnEndNode, TranscriptNode>()

function withProduced(endNode: TurnEndNode, span: TranscriptNode[]): TranscriptNode {
  const cached = producedTurnCache.get(endNode)
  if (cached) return cached
  const files: TurnProducedFile[] = []
  const byPath = new Map<string, TurnProducedFile>()
  for (const node of span) {
    if (node.kind !== 'tool') continue
    const info = toolProducedInfo(node)
    if (info === null) continue
    const existing = byPath.get(info.path)
    if (existing === undefined) {
      const file: TurnProducedFile = {
        path: info.path,
        kind: info.kind,
        added: info.added,
        removed: info.removed,
        diffs: info.diffs.slice(0, PRODUCED_DIFF_FRAMES),
      }
      byPath.set(info.path, file)
      files.push(file)
      continue
    }
    existing.added += info.added
    existing.removed += info.removed
    if (existing.diffs.length < PRODUCED_DIFF_FRAMES) {
      existing.diffs.push(...info.diffs.slice(0, PRODUCED_DIFF_FRAMES - existing.diffs.length))
    }
  }
  const next: TranscriptNode =
    files.length === 0 ? endNode : { ...endNode, produced: files }
  producedTurnCache.set(endNode, next)
  return next
}

/**
 * Groups transcript nodes for display. A closed turn folds everything through
 * its last tool call (inclusive) into one "worked X · N tool calls" overview
 * row, keeping only the answer that follows it visible; genuine user messages
 * stay in place. An open (running) turn is left flat: tool calls render as
 * individual rows so the reader always sees live progress.
 */
export function groupTranscript(nodes: TranscriptNode[]): TranscriptRow[] {
  const rows: TranscriptRow[] = []
  let index = 0
  while (index < nodes.length) {
    const marker = nodes[index]
    if (marker.kind !== 'turn-start') {
      rows.push({ kind: 'node', node: marker })
      index += 1
      continue
    }
    let end = -1
    for (let j = index + 1; j < nodes.length; j++) {
      if (nodes[j].kind === 'turn-end') {
        end = j
        break
      }
    }
    if (end < 0) {
      for (const node of nodes.slice(index + 1)) {
        if (node.kind !== 'turn-start') rows.push({ kind: 'node', node })
      }
      break
    }
    const span = nodes.slice(index + 1, end)
    const endNode = nodes[end] as TurnEndNode
    rows.push(...closedTurnRows(span, marker, endNode))
    rows.push({ kind: 'node', node: withProduced(endNode, span) })
    index = end + 1
  }
  return rows
}

/** Fold one closed turn's prefix *through* its last tool call into an overview row. */
function closedTurnRows(
  span: TranscriptNode[],
  marker: Extract<TranscriptNode, { kind: 'turn-start' }>,
  endNode: Extract<TranscriptNode, { kind: 'turn-end' }>,
): TranscriptRow[] {
  let lastTool = -1
  for (let i = span.length - 1; i >= 0; i--) {
    if (span[i].kind === 'tool') {
      lastTool = i
      break
    }
  }
  if (lastTool < 0) return span.map((node) => ({ kind: 'node', node }) as TranscriptRow)
  // 折叠窗口含最后一条工具调用(用户规格:最后一条工具调用"以上"全部折叠),
  // 可见部分只剩其后的最终回答。
  const foldEnd = lastTool + 1
  const prefix = span.slice(0, foldEnd)
  // 用户消息与**父代理来信**一样是对话,必须留在原位(任务正文被藏进
  // "worked X" 概览里,子代理视角下就读不到主代理到底交代了什么)。
  const inPlace = (node: TranscriptNode) =>
    node.kind === 'user' || (node.kind === 'context-injection' && node.fromParentAgent === true)
  const hidden = prefix.filter((node) => !inPlace(node))
  // The overview is inserted where the first folded row would sit; when there
  // is nothing expandable, no overview is shown at all.
  const overview: TranscriptRow | null = hidden.some(rendersContent)
    ? {
      kind: 'overview',
      durationMs: Math.max(0, endNode.time - marker.time),
      toolCount: span.reduce((count, node) => count + (node.kind === 'tool' ? 1 : 0), 0),
      hidden,
    }
    : null
  const rows: TranscriptRow[] = []
  let placed = false
  for (const node of prefix) {
    const foldable = !inPlace(node)
    if (!foldable || overview === null) {
      rows.push({ kind: 'node', node })
      continue
    }
    if (!placed) {
      rows.push(overview)
      placed = true
    }
  }
  if (overview !== null && !placed) rows.push(overview)
  rows.push(...span.slice(foldEnd).map((node) => ({ kind: 'node', node }) as TranscriptRow))
  return rows
}

export function assistantHasContent(node: Extract<TranscriptNode, { kind: 'assistant' }>): boolean {
  return node.interrupted || node.blocks.some((block) =>
    block.kind !== 'tool-call' && block.text.trim().length > 0)
}

/** Whether a hidden node would paint anything when expanded. */
function rendersContent(node: TranscriptNode): boolean {
  if (
    node.kind === 'tool' ||
    node.kind === 'user' ||
    node.kind === 'context-injection' ||
    node.kind === 'system-prompt' ||
    node.kind === 'compacting'
  ) {
    return true
  }
  if (node.kind === 'assistant') {
    return assistantHasContent(node)
  }
  return false
}
