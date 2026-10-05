import { useCallback, useEffect, useMemo, useRef, useState, type RefObject, type MutableRefObject, type Dispatch, type SetStateAction } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { activeSlashToken } from '../../pages/slash'
import { caretOffsetAtPoint, caretOffsetIn, caretRightAfterChip, createSlashChip, renderDraft, selectRange, serializeEditor, setCaretOffset, type SlashChipKind } from '../../pages/editor'
import { REFERENCE_MIME, decodeReference, insertReferenceAt, referenceText } from '../../fileTree'

interface Options {
  activeId: string | null
  inert: boolean
  prompt: string
  promptRef: RefObject<HTMLDivElement>
  setPrompt: Dispatch<SetStateAction<string>>
  setOptimizedPrompt: Dispatch<SetStateAction<string | null>>
  originalPromptRef: MutableRefObject<string>
  syncPromptHeight: () => void
  closeMention: () => void
  refreshMentions: (text: string, caret: number) => void
  composingRef: MutableRefObject<boolean>
}
export function useComposerSlash({ activeId, inert, prompt, promptRef, setPrompt, setOptimizedPrompt,
  originalPromptRef, syncPromptHeight, closeMention, refreshMentions, composingRef }: Options) {
  /* ---- 输入框 `/` 斜杠命令/技能(对照 dsh input-trigger:命令认领行,技能直调注入) ---- */

  /** 弹层候选(命令/技能统一结构,description 为已翻译文案)。 */
  interface SlashCandidate {
    name: string
    kind: 'command' | 'skill'
    description: string
    /** 技能仅用户可直调(disable-model-invocation)。 */
    userOnly: boolean
  }

  const [slashOpen, setSlashOpen] = useState(false)
  const [slashItems, setSlashItems] = useState<SlashCandidate[]>([])
  const [slashActive, setSlashActive] = useState(0)
  const slashMenuRef = useRef<HTMLDivElement | null>(null)
  // 技能直调词典(仅 user-invocable;仅模型的直调会被后端 400,不进候选)。
  const [skillEntries, setSkillEntries] = useState<api.SkillSummary[]>([])
  // token 词典(命令 + 技能):候选过滤、卡片重建、发送收集共用,kind 决定
  // 卡片样式身份。
  const slashKinds = useMemo(() => {
    const map = new Map<string, SlashChipKind>()
    map.set('plan', 'command')
    map.set('compact', 'command')
    for (const skill of skillEntries) map.set(skill.name, 'skill')
    return map
  }, [skillEntries])
  const slashKindRef = useRef(slashKinds)
  slashKindRef.current = slashKinds

  /** 外部写路径:把草稿文本重建进编辑器 DOM(命中词典的 token 重建为卡片)。 */
  const applyDraft = useCallback((text: string) => {
    setPrompt(text)
    setOptimizedPrompt(null)
    originalPromptRef.current = ''
    const el = promptRef.current
    if (el) {
      renderDraft(el, text, slashKindRef.current)
      syncPromptHeight()
    }
  }, [])

  const closeSlash = useCallback(() => {
    setSlashOpen(false)
    setSlashItems([])
  }, [])

  /**
   * 从侧栏「工作区文件」树拖入的引用:在鼠标落点插入 `@路径`。
   *
   * 结果与手打 `@` 选中候选**完全一致**(共用 `formatFileMention` 的格式化),
   * 所以模型侧看到的引用语法只有一种形态。落点用 `caretOffsetAtPoint` 解析;
   * 解析不出来(拖到了输入框的空白边距、或浏览器不支持该 API)就插到**末尾**
   * —— 那仍是用户期望的"加进这段草稿",而丢弃这次拖拽会让操作看起来失效。
   */
  const onEditorDrop = useCallback(
    (event: React.DragEvent<HTMLDivElement>) => {
      const raw = event.dataTransfer.getData(REFERENCE_MIME)
      if (!raw) return
      const payload = decodeReference(raw)
      if (!payload) return
      event.preventDefault()
      const reference = referenceText(payload)
      if (!reference) return
      const el = promptRef.current
      if (!el) return
      const draft = serializeEditor(el)
      const at = caretOffsetAtPoint(el, event.clientX, event.clientY) ?? draft.length
      const { text, caret } = insertReferenceAt(draft, at, reference)
      applyDraft(text)
      setCaretOffset(el, caret)
      closeMention()
      closeSlash()
      el.focus()
    },
    [applyDraft, closeMention, closeSlash],
  )

  /** 检测光标处 `/` token 并按词典出候选(同步,无网络);@ 提及优先,二者互斥。 */
  const refreshSlash = useCallback(
    (text: string, caret: number) => {
      if (inert) {
        closeSlash()
        return
      }
      const el = promptRef.current
      // 光标紧贴卡片之后时,序列化里的 `/name` 是卡片的一部分,不再触发弹层。
      if (!el || caretRightAfterChip(el)) {
        closeSlash()
        return
      }
      const token = activeSlashToken(text, caret)
      if (!token) {
        closeSlash()
        return
      }
      const query = token.query.toLowerCase()
      const matches = (entry: SlashCandidate) => !query || entry.name.toLowerCase().includes(query)
      // 组内前缀命中排在子串命中前;命令组恒在技能组前(命令优先于技能)。
      const ordered = (list: SlashCandidate[]) => [
        ...list.filter((entry) => !query || entry.name.toLowerCase().startsWith(query)),
        ...list.filter((entry) => query && !entry.name.toLowerCase().startsWith(query)),
      ]
      const commands: SlashCandidate[] = [
        { name: 'plan', kind: 'command', description: t('cmdPlanDesc'), userOnly: false },
        { name: 'compact', kind: 'command', description: t('cmdCompactDesc'), userOnly: false },
        { name: 'goal', kind: 'command', description: t('cmdGoalDesc'), userOnly: false },
      ]
      const skills: SlashCandidate[] = skillEntries.map((skill) => ({
        name: skill.name,
        kind: 'skill',
        description: skill.description,
        userOnly: !skill.modelInvocable,
      }))
      setSlashItems([...ordered(commands.filter(matches)), ...ordered(skills.filter(matches))])
      setSlashActive(0)
      setSlashOpen(true)
    },
    [inert, skillEntries, closeSlash],
  )

  /**
   * 选中候选:先把 token 区间选中,insertHTML 换成卡片原子元素(原生 undo
   * 可整体撤销)。Chromium 的 insertHTML 会把光标落在不可编辑元素之前,所以
   * 插完显式钉到卡片之后再补空格,保证「卡片 + 空格」顺序与光标落点正确。
   */
  /**
   * 手动压缩的转发 ref:`pickSlash` 定义在 `runCompact` 之前(菜单逻辑靠
   * 前),用 ref 拿到最新实现,免得把整块压缩逻辑提前搬上来。
   */
  const runCompactRef = useRef<(id: string) => Promise<void>>(async () => {})

  const pickSlash = useCallback(
    (candidate: SlashCandidate) => {
      // 压缩是"立即执行"型命令:点选即发起,不落回输入框再等一次回车
      // (它不产生消息、不经过模型,插进编辑器只会多一次无谓的确认)。
      if (candidate.name === 'compact' && candidate.kind === 'command') {
        // 触发菜单的那个 `/` 必须一起删掉:否则命令执行了,输入框里还留
        // 着半个斜杠(用户还得手动退格)。
        const editor = promptRef.current
        if (editor) {
          const text = serializeEditor(editor)
          const caret = caretOffsetIn(editor)
          const token = activeSlashToken(text, caret)
          if (token) {
            selectRange(editor, caret - token.prefix.length, caret)
            document.execCommand('delete')
          }
          setPrompt(serializeEditor(editor))
          originalPromptRef.current = ''
          syncPromptHeight()
        }
        closeSlash()
        if (activeId) void runCompactRef.current(activeId)
        return
      }
      const el = promptRef.current
      if (!el || document.activeElement !== el) return
      const text = serializeEditor(el)
      const caret = caretOffsetIn(el)
      const token = activeSlashToken(text, caret)
      if (!token) return
      const start = caret - token.prefix.length
      const after = text.slice(caret)
      const trailing = after.length === 0 || !/^\s/u.test(after) ? ' ' : ''
      selectRange(el, start, caret)
      document.execCommand('insertHTML', false, createSlashChip(candidate.name, candidate.kind).outerHTML)
      setCaretOffset(el, start + candidate.name.length + 1)
      if (trailing) {
        document.execCommand('insertText', false, trailing)
        setCaretOffset(el, start + candidate.name.length + 1 + trailing.length)
      }
      setPrompt(serializeEditor(el))
      setOptimizedPrompt(null)
      originalPromptRef.current = ''
      closeSlash()
      syncPromptHeight()
    },
    [closeSlash, runCompactRef],
  )

  /** input/selectionchange 共用的菜单触发检测:序列化草稿 + 光标偏移。 */
  const refreshMenus = useCallback(() => {
    const el = promptRef.current
    if (!el) return
    const text = serializeEditor(el)
    const caret = caretOffsetIn(el)
    refreshMentions(text, caret)
    refreshSlash(text, caret)
  }, [refreshMentions, refreshSlash])
  const refreshMenusRef = useRef(refreshMenus)
  refreshMenusRef.current = refreshMenus

  // 光标移动(点击/方向键/Home/End)不产生 input 事件:用 selectionchange
  // 驱动 @ 与 / 的触发检测。卡片原子性由 contenteditable=false 原生保证。
  useEffect(() => {
    const onSelectionChange = () => {
      const el = promptRef.current
      if (!el || document.activeElement !== el || composingRef.current) return
      refreshMenusRef.current()
    }
    document.addEventListener('selectionchange', onSelectionChange)
    return () => document.removeEventListener('selectionchange', onSelectionChange)
  }, [])

  // 键盘高亮条目跟随滚动(与 @ 提及菜单同款 data-kb 标记)。
  useEffect(() => {
    if (!slashOpen) return
    slashMenuRef.current?.querySelector('[data-kb="true"]')?.scrollIntoView({ block: 'nearest' })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [slashOpen, slashActive, slashItems])

  // 发送后输入框被程序清空(不走 onChange):兜底收起弹层。
  useEffect(() => {
    if (slashOpen && !prompt) closeSlash()
  }, [slashOpen, prompt, closeSlash])

  // 会话切换时刷新技能词典(装饰与候选共用);失败静默,菜单仍有命令组。
  useEffect(() => {
    if (!activeId) {
      setSkillEntries([])
      return
    }
    const controller = new AbortController()
    api
      .listSkills(activeId, controller.signal)
      .then(({ skills }) => { if (!controller.signal.aborted) setSkillEntries(skills.filter((skill) => skill.userInvocable)) })
      .catch(() => {})
    return () => controller.abort()
  }, [activeId])


  return { slashOpen, slashItems, slashActive, setSlashActive, slashMenuRef, skillEntries,
    applyDraft, closeSlash, refreshMenus, pickSlash, onEditorDrop, runCompactRef }
}
