import { useCallback, useEffect, useRef, useState, type RefObject } from 'react'
import * as api from '../../api'
import type { MentionCandidate } from '../../api'
import { activeAtToken, formatFileMention } from '../../pages/mention'
import { caretOffsetIn, selectRange, serializeEditor } from '../../pages/editor'

interface Options {
  mentionCwd: string | null
  promptRef: RefObject<HTMLDivElement>
  onDraftChange: (text: string) => void
  syncPromptHeight: () => void
}
export function useComposerMentions({ mentionCwd, promptRef, onDraftChange, syncPromptHeight }: Options) {
  /* ---- 输入框 @ 文件/文件夹提及(对照 dsh file-reference:候选只含路径,内容留在 read 工具) ---- */

  const [mentionOpen, setMentionOpen] = useState(false)
  const [mentionItems, setMentionItems] = useState<MentionCandidate[]>([])
  const [mentionLoading, setMentionLoading] = useState(false)
  /** 当前查询串(空态提示区分"根目录为空"与"无匹配")。 */
  const [mentionQuery, setMentionQuery] = useState('')
  const [mentionActive, setMentionActive] = useState(0)
  const mentionMenuRef = useRef<HTMLDivElement | null>(null)
  const mentionDebounceRef = useRef<number | null>(null)
  const mentionAbortRef = useRef<AbortController | null>(null)
  // IME 组词阶段不触发检测/选择旁路(对照模型菜单的 isComposing 旁路)。
  // 候选范围 = 已存活会话的 cwd;无会话或 cwd 失效时不触发。
  // 下钻时显示当前位置(query 含 / 时取目录前缀),对照 dsh 的面包屑头部。
  const mentionDir = mentionQuery.includes('/')
    ? mentionQuery.slice(0, mentionQuery.lastIndexOf('/') + 1)
    : ''

  const closeMention = useCallback(() => {
    if (mentionDebounceRef.current !== null) {
      window.clearTimeout(mentionDebounceRef.current)
      mentionDebounceRef.current = null
    }
    mentionAbortRef.current?.abort()
    mentionAbortRef.current = null
    setMentionOpen(false)
    setMentionItems([])
    setMentionLoading(false)
  }, [])

  /** 检测光标处 `@` token 并按 150ms 防抖拉候选;token 消失/无 cwd 时关闭。 */
  const refreshMentions = useCallback(
    (text: string, caret: number) => {
      const cwd = mentionCwd
      if (!cwd) {
        closeMention()
        return
      }
      const token = activeAtToken(text, caret)
      if (!token) {
        closeMention()
        return
      }
      mentionAbortRef.current?.abort()
      setMentionQuery(token.query)
      setMentionOpen(true)
      setMentionLoading(true)
      if (mentionDebounceRef.current !== null) window.clearTimeout(mentionDebounceRef.current)
      mentionDebounceRef.current = window.setTimeout(() => {
        mentionDebounceRef.current = null
        mentionAbortRef.current?.abort()
        const controller = new AbortController()
        mentionAbortRef.current = controller
        api
          .searchMentions(cwd, token.query, controller.signal)
          .then(({ items }) => {
            if (controller.signal.aborted) return
            setMentionItems(items)
            setMentionActive(0)
            setMentionLoading(false)
          })
          .catch(() => {
            // 后端 400(目录已死等):按无结果收起候选,不打断输入。
            if (!controller.signal.aborted) {
              setMentionItems([])
              setMentionLoading(false)
            }
          })
      }, 150)
    },
    [mentionCwd, closeMention],
  )

  /** 选中候选:用格式化文本替换当前 token,补空格,光标随插入内容其后。 */
  const pickMention = useCallback(
    (candidate: MentionCandidate) => {
      const el = promptRef.current
      if (!el || document.activeElement !== el) return
      const text = serializeEditor(el)
      const caret = caretOffsetIn(el)
      const token = activeAtToken(text, caret)
      if (!token) return
      const mention = formatFileMention(candidate, token.quoted)
      if (mention === undefined) return
      const after = text.slice(caret)
      const suffix = after.length === 0 || !/^\s/u.test(after) ? ' ' : ''
      selectRange(el, caret - token.prefix.length, caret)
      document.execCommand('insertText', false, `${mention}${suffix}`)
      onDraftChange(serializeEditor(el))
      closeMention()
      syncPromptHeight()
    },
    [closeMention, onDraftChange, promptRef, syncPromptHeight],
  )

  // 键盘高亮条目跟随滚动(与模型菜单同款 data-kb 标记)。
  useEffect(() => {
    if (!mentionOpen) return
    mentionMenuRef.current?.querySelector('[data-kb="true"]')?.scrollIntoView({ block: 'nearest' })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mentionOpen, mentionActive, mentionItems])

  // cwd 失效/会话切换时确保菜单关闭。
  useEffect(() => {
    closeMention()
  }, [mentionCwd, closeMention])

  // 卸载兜底:防抖与在途请求随组件销毁取消。
  useEffect(() => {
    return () => {
      if (mentionDebounceRef.current !== null) window.clearTimeout(mentionDebounceRef.current)
      mentionAbortRef.current?.abort()
    }
  }, [])

  return { mentionOpen, mentionItems, mentionLoading, mentionQuery, mentionActive, setMentionActive,
    mentionMenuRef, mentionDir, closeMention, refreshMentions, pickMention }
}
