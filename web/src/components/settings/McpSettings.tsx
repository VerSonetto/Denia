import { useCallback, useEffect, useMemo, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { ConfirmDialog } from '../ConfirmDialog'
import { Badge, Button, Field, TextInput } from '../llm/atoms/form'
import styles from './McpSettings.module.css'

/** 表单 / JSON 双模;JSON 模式用于直接粘贴一段 dsh 风格配置。 */
type EditorMode = 'form' | 'json'

/** 作用范围:与 dsh UI 对齐;本期只接入 user(全局),project 字段已存,
 * 后端按 user 同样处理以避免半成品行为(UI 提示用户当前生效范围)。 */
type ScopeValue = 'user' | 'project'

/** 协议版本白名单:当前只接受自动协商,但保留 UI 入口与字段以便升级。 */
const PROTOCOL_VERSION_OPTIONS: { id: string; label: string }[] = [
  { id: 'auto', label: '自动(推荐)' },
  { id: '2024-11-05', label: '2024-11-05' },
]

/** 传输方式:stdio(本地命令)/ http(Streamable HTTP)/ sse(HTTP+SSE)。 */
type TransportValue = 'stdio' | 'http' | 'sse'

/** 编辑态:同时容纳两种模式(`mode === 'json'` 时只看 `rawJson`)。 */
interface EditorState {
  mode: EditorMode
  scope: ScopeValue
  transport: TransportValue
  // 表单态字段(都是字符串,提交时再做合法化)。
  id: string
  name: string
  command: string
  args: string
  envText: string
  cwd: string
  // http / sse 专用。
  url: string
  headersText: string
  protocolVersion: string
  timeoutMs: string
  enabled: boolean
  // 原始 draft(用于错误展示/模式切换回退)。
  original: api.McpServerDraft | null
  // JSON 模式下的文本。
  rawJson: string
  // 表单/JSON 各自一份解析错误。
  error: string
}

function emptyEditor(): EditorState {
  return {
    mode: 'form',
    scope: 'user',
    transport: 'stdio',
    id: '',
    name: '',
    command: '',
    args: '',
    envText: '',
    cwd: '',
    url: '',
    headersText: '',
    protocolVersion: 'auto',
    timeoutMs: '30000',
    enabled: true,
    original: null,
    rawJson: '',
    error: '',
  }
}

const ID_PATTERN = /^[a-z][a-z0-9-]*$/

/** 把参数串按引号/空格拆成数组(供表单态 args 字段解析)。 */
function splitArgs(raw: string): string[] {
  const out: string[] = []
  let current = ''
  let quote: '"' | "'" | null = null
  let hasToken = false
  for (let index = 0; index < raw.length; index += 1) {
    const char = raw[index]
    if (quote) {
      if (char === quote) {
        quote = null
      } else {
        current += char
      }
      continue
    }
    if (char === '"' || char === "'") {
      quote = char
      hasToken = true
      continue
    }
    if (/\s/.test(char)) {
      if (hasToken) out.push(current)
      current = ''
      hasToken = false
      continue
    }
    if (char === '\\' && /\s/.test(raw[index + 1] ?? '')) {
      current += raw[index + 1]
      index += 1
      hasToken = true
      continue
    }
    current += char
    hasToken = true
  }
  if (hasToken) out.push(current)
  return out
}

/** env 是「KEY=VALUE」行列表,空行 / `#` 注释跳过(与 .env 同形态)。 */
function parseEnvLines(text: string): Record<string, string> {
  const env: Record<string, string> = {}
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim()
    if (!line || line.startsWith('#')) continue
    const eq = line.indexOf('=')
    if (eq < 0) continue
    const key = line.slice(0, eq).trim()
    if (!key) continue
    const value = line.slice(eq + 1).trim()
    env[key] = value
  }
  return env
}

/** 把 Record 序列化回 KEY=VALUE 行,空值保留空字符串(给用户重填)。 */
function formatEnvLines(env: Record<string, string>): string {
  return Object.entries(env)
    .map(([key, value]) => `${key}=${value}`)
    .join('\n')
}

/**
 * 把"一段 dsh 风格的 JSON 配置"解析为单个服务器草稿。
 *
 * 支持两种顶层形态(对齐 dsh UI 文案):
 * - `{ "my-mcp-server": { "type": "stdio", "command": "...", "args": [...] } }`
 * - `{ "mcpServers": { "server-name": { ... } } }`
 *
 * 解析失败返回错误文本,由 UI 提示,不会让坏值进 settings。
 */
function parseDshJson(raw: string): { draft: api.McpServerDraft; error: string | null } {
  const trimmed = raw.trim()
  if (!trimmed) {
    return { draft: emptyDraftFromJson(), error: '请粘贴 JSON 配置' }
  }
  let value: unknown
  try {
    value = JSON.parse(trimmed)
  } catch (cause) {
    return { draft: emptyDraftFromJson(), error: `JSON 解析失败:${(cause as Error).message}` }
  }
  if (!value || typeof value !== 'object') {
    return { draft: emptyDraftFromJson(), error: 'JSON 必须是对象' }
  }
  // 形态一:mcpServers 包裹
  let servers: Record<string, unknown> | null = null
  const obj = value as Record<string, unknown>
  if (obj.mcpServers && typeof obj.mcpServers === 'object') {
    servers = obj.mcpServers as Record<string, unknown>
  } else {
    servers = obj
  }
  // 取第一个键(配置项通常只有一个,但有多个就给提示)
  const ids = Object.keys(servers)
  if (ids.length === 0) {
    return { draft: emptyDraftFromJson(), error: 'JSON 中找不到任何服务器定义' }
  }
  if (ids.length > 1) {
    return { draft: emptyDraftFromJson(), error: '一次只能保存一个服务器;请只保留一个键' }
  }
  const id = ids[0]
  const def = servers[id]
  if (!def || typeof def !== 'object') {
    return { draft: emptyDraftFromJson(), error: `服务器 '${id}' 的定义不是对象` }
  }
  const cfg = def as Record<string, unknown>
  // 类型:stdio / http / sse;http/sse 必须给 url。
  const type = (cfg.type as string) ?? 'stdio'
  if (!['stdio', 'http', 'sse'].includes(type)) {
    return {
      draft: emptyDraftFromJson(),
      error: `传输类型 '${type}' 不支持;可用:stdio / http / sse`,
    }
  }
  const command = ((cfg.command as string) ?? '').trim()
  const url = (cfg.url as string) ?? ''
  if (type === 'stdio' && !command) {
    return { draft: emptyDraftFromJson(), error: `服务器 '${id}' 缺少 command` }
  }
  if (type !== 'stdio' && !/^https?:\/\//.test(url)) {
    return {
      draft: emptyDraftFromJson(),
      error: `服务器 '${id}' 是 ${type} 传输,url 必须是 http/https 地址`,
    }
  }
  const args = Array.isArray(cfg.args)
    ? cfg.args.map((arg) => String(arg))
    : []
  const env: Record<string, string> = {}
  if (cfg.env && typeof cfg.env === 'object') {
    for (const [key, value] of Object.entries(cfg.env as Record<string, unknown>)) {
      env[key] = String(value ?? '')
    }
  }
  return {
    draft: {
      id,
      transport: type,
      command,
      args,
      env,
      url: (cfg.url as string) ?? null,
      headers: cfg.headers && typeof cfg.headers === 'object'
        ? Object.fromEntries(
            Object.entries(cfg.headers as Record<string, unknown>).map(([k, v]) => [
              k,
              String(v ?? ''),
            ]),
          )
        : {},
      cwd: (cfg.cwd as string) ?? null,
      enabled: cfg.enabled === undefined ? true : Boolean(cfg.enabled),
      disabledTools: Array.isArray(cfg.disabledTools)
        ? (cfg.disabledTools as unknown[]).map((tool) => String(tool))
        : [],
    },
    error: null,
  }
}

function emptyDraftFromJson(): api.McpServerDraft {
  return {
    id: '',
    transport: 'stdio',
    command: '',
    args: [],
    env: {},
    url: null,
    headers: {},
    cwd: null,
    enabled: true,
    disabledTools: [],
  }
}

/** 把 draft 序列化成 dsh 风格的 JSON 文本(用作 JSON 模式的初值)。 */
function draftToJson(draft: api.McpServerDraft): string {
  const def: Record<string, unknown> = {
    type: draft.transport,
    command: draft.command,
    args: draft.args,
  }
  if (draft.env && Object.keys(draft.env).length > 0) def.env = draft.env
  if (draft.cwd) def.cwd = draft.cwd
  if (!draft.enabled) def.enabled = false
  if (draft.disabledTools.length > 0) def.disabledTools = draft.disabledTools
  // 键即服务器 id;空 id 时给空串作占位,提示用户替换。
  const obj: Record<string, unknown> = { [draft.id]: def }
  return JSON.stringify(obj, null, 2)
}

export function McpSettings({ notify }: { notify: (kind: 'ok' | 'err', text: string) => void }) {
  const [data, setData] = useState<api.McpDescribe | null>(null)
  const [loading, setLoading] = useState(true)
  const [busyId, setBusyId] = useState<string | null>(null)
  const [error, setError] = useState('')

  // 编辑态:null = 关闭对话框。
  const [editing, setEditing] = useState<EditorState | null>(null)
  const [expanded, setExpanded] = useState<string | null>(null)
  const [pendingDelete, setPendingDelete] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      const next = await api.getMcp()
      setData(next)
      setError('')
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  const applyResult = (next: api.McpDescribe) => {
    setData(next)
    setError('')
  }

  const runServerAction = async (
    id: string,
    action: 'enable' | 'disable' | 'reconnect',
  ) => {
    setBusyId(id)
    try {
      applyResult(await api.mcpServerAction(id, action))
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause)
      setError(message)
      notify('err', message)
    } finally {
      setBusyId(null)
    }
  }

  const removeServer = async (id: string) => {
    setPendingDelete(null)
    setBusyId(id)
    try {
      applyResult(await api.deleteMcpServer(id))
      notify('ok', t('mcpServerDeleted'))
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause)
      setError(message)
      notify('err', message)
    } finally {
      setBusyId(null)
    }
  }

  const toggleTool = async (server: string, tool: string, enabled: boolean) => {
    setBusyId(`${server}/${tool}`)
    try {
      applyResult(await api.toggleMcpTool(server, tool, enabled))
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause)
      setError(message)
      notify('err', message)
    } finally {
      setBusyId(null)
    }
  }

  /** 打开「新建」对话框。 */
  const startCreate = () => {
    const next = emptyEditor()
    next.rawJson = draftToJson(emptyDraftFromJson())
    setEditing(next)
  }

  /** 打开「编辑」对话框:env 值不回传,留空让用户重填。 */
  const startEdit = (server: api.McpServerInfo) => {
    const draft: api.McpServerDraft = {
      id: server.id,
      transport: server.transport,
      command: server.command,
      args: server.args,
      env: {},
      cwd: server.cwd ?? null,
      enabled: server.enabled,
      disabledTools: server.tools.filter((tool) => !tool.enabled).map((tool) => tool.name),
    }
    const next: EditorState = {
      mode: 'form',
      scope: 'user',
      transport: (draft.transport as TransportValue) ?? 'stdio',
      id: draft.id,
      name: draft.id,
      command: draft.command,
      args: draft.args.join(' '),
      envText: '',
      cwd: draft.cwd ?? '',
      url: draft.url ?? '',
      headersText: '',
      protocolVersion: 'auto',
      timeoutMs: '30000',
      enabled: draft.enabled,
      original: draft,
      rawJson: draftToJson(draft),
      error: '',
    }
    setEditing(next)
  }

  /** 切换模式:JSON → 表单 自动尝试反填;表单 → JSON 用 draftToJson。 */
  const switchMode = (nextMode: EditorMode) => {
    if (!editing) return
    if (editing.mode === nextMode) return
    if (nextMode === 'form') {
      const { draft, error } = parseDshJson(editing.rawJson)
      if (error) {
        setEditing({ ...editing, error: `无法从 JSON 反填到表单:${error}` })
        return
      }
      setEditing({
        ...editing,
        mode: 'form',
        id: draft.id,
        name: draft.id,
        transport: (draft.transport as TransportValue) ?? 'stdio',
        command: draft.command,
        args: draft.args.join(' '),
        envText: formatEnvLines(draft.env),
        cwd: draft.cwd ?? '',
        url: draft.url ?? '',
        headersText: formatEnvLines(draft.headers ?? {}),
        enabled: draft.enabled,
        original: editing.original,
        error: '',
      })
    } else {
      // 表单 → JSON:只做数据搬运,**不做校验**——用户可能正要切过去
      // 补填(id/命令还没填是允许的)。校验留给保存时统一做。
      const draft = draftFromForm(editing)
      setEditing({ ...editing, mode: 'json', rawJson: draftToJson(draft), error: '' })
    }
  }

  /// 表单态 → 草稿(纯搬运,不校验)。切到 JSON 模式时用它。
  const draftFromForm = (state: EditorState): api.McpServerDraft => ({
    id: state.id.trim(),
    transport: state.transport,
    command: state.command.trim(),
    args: splitArgs(state.args),
    env: parseEnvLines(state.envText),
    url: state.url.trim() || null,
    headers: parseEnvLines(state.headersText),
    cwd: state.cwd.trim() || null,
    enabled: state.enabled,
    disabledTools: [],
  })

  /// 保存前校验:不合法时把错误写回 `editing.error` 并返回 null。
  const buildDraftFromForm = (state: EditorState): api.McpServerDraft | null => {
    if (!ID_PATTERN.test(state.id.trim())) {
      setEditing({ ...state, error: t('mcpErrorId') })
      return null
    }
    if (state.transport === 'stdio' && !state.command.trim()) {
      setEditing({ ...state, error: t('mcpErrorCommand') })
      return null
    }
    if (state.transport !== 'stdio' && !/^https?:\/\//.test(state.url.trim())) {
      setEditing({ ...state, error: t('mcpErrorUrl') })
      return null
    }
    return draftFromForm(state)
  }

  const submit = async () => {
    if (!editing) return
    const draft =
      editing.mode === 'form'
        ? buildDraftFromForm(editing)
        : (() => {
            const { draft, error } = parseDshJson(editing.rawJson)
            if (error) {
              setEditing({ ...editing, error })
              return null
            }
            if (!ID_PATTERN.test(draft.id)) {
              setEditing({ ...editing, error: t('mcpErrorId') })
              return null
            }
            return draft
          })()
    if (!draft) return
    setBusyId('form')
    setEditing({ ...editing, error: '' })
    try {
      applyResult(await api.saveMcpServer(draft))
      setEditing(null)
      notify('ok', t('mcpServerSaved'))
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause)
      setEditing({ ...editing, error: message })
      notify('err', message)
    } finally {
      setBusyId(null)
    }
  }

  const totalTools = useMemo(() => data?.toolCount ?? 0, [data])

  if (loading) {
    return (
      <section className="setm-section">
        <p className={styles.hint}>{t('loading')}</p>
      </section>
    )
  }

  return (
    <section className="setm-section">
      {error && (
        <p className={styles.error} role="alert">
          {error}
        </p>
      )}

      <header className={styles.head}>
        <div>
          <p className={styles.summary}>
            {t('mcpServerCount', { count: data?.servers.length ?? 0 })} ·{' '}
            {t('mcpToolCount', { count: totalTools })}
          </p>
          <p className={styles.hint}>{t('mcpPaneDesc')}</p>
        </div>
        <Button variant="primary" small onClick={startCreate} disabled={editing != null}>
          {t('mcpAddServer')}
        </Button>
      </header>

      {editing && (
        <div className={styles.modalBackdrop} onClick={() => setEditing(null)}>
          <div
            className={styles.modal}
            role="dialog"
            aria-modal="true"
            aria-label={t('mcpEditorTitle')}
            onClick={(event) => event.stopPropagation()}
          >
            <header className={styles.modalHead}>
              <div>
                <h3 className={styles.modalTitle}>{t('mcpEditorTitle')}</h3>
                <p className={styles.hint}>{t('mcpEditorDesc')}</p>
              </div>
              <div className={styles.modeSwitch} role="tablist">
                <button
                  type="button"
                  role="tab"
                  aria-selected={editing.mode === 'form'}
                  className={`${styles.modeBtn}${editing.mode === 'form' ? ` ${styles.active}` : ''}`}
                  onClick={() => switchMode('form')}
                >
                  {t('mcpModeForm')}
                </button>
                <button
                  type="button"
                  role="tab"
                  aria-selected={editing.mode === 'json'}
                  className={`${styles.modeBtn}${editing.mode === 'json' ? ` ${styles.active}` : ''}`}
                  onClick={() => switchMode('json')}
                >
                  {t('mcpModeJson')}
                </button>
              </div>
            </header>

            <div className={styles.scopeRow}>
              <span className={styles.scopeLabel}>{t('mcpScopeLabel')}</span>
              <div className={styles.scopeChips} role="radiogroup">
                {(
                  [
                    { id: 'user', label: t('mcpScopeUser') },
                    { id: 'project', label: t('mcpScopeProject') },
                  ] as { id: ScopeValue; label: string }[]
                ).map((option) => (
                  <button
                    key={option.id}
                    type="button"
                    role="radio"
                    aria-checked={editing.scope === option.id}
                    className={`${styles.scopeChip}${editing.scope === option.id ? ` ${styles.active}` : ''}`}
                    onClick={() => setEditing({ ...editing, scope: option.id })}
                  >
                    {option.label}
                  </button>
                ))}
              </div>
              <p className={styles.hint}>
                {editing.scope === 'user'
                  ? t('mcpScopeUserHint')
                  : t('mcpScopeProjectHint')}
              </p>
            </div>

            {editing.mode === 'form' ? (
              <form
                className={styles.modalBody}
                onSubmit={(event) => {
                  event.preventDefault()
                  void submit()
                }}
              >
                <Field label={t('mcpName')} error={null}>
                  <TextInput
                    value={editing.name}
                    onChange={(next) =>
                      setEditing({ ...editing, name: next, id: next.trim() })
                    }
                    placeholder={t('mcpServerIdPlaceholder')}
                    disabled={editing.original != null}
                  />
                </Field>
                <Field label={t('mcpType')} error={null}>
                  <div className={styles.typeChips} role="radiogroup">
                    {(
                      [
                        { id: 'stdio', label: t('mcpTypeStdio') },
                        { id: 'http', label: t('mcpTypeHttp') },
                        { id: 'sse', label: t('mcpTypeSse') },
                      ] as { id: TransportValue; label: string }[]
                    ).map((option) => (
                      <button
                        key={option.id}
                        type="button"
                        role="radio"
                        aria-checked={editing.transport === option.id}
                        className={`${styles.typeChip}${editing.transport === option.id ? ` ${styles.active}` : ''}`}
                        onClick={() => setEditing({ ...editing, transport: option.id })}
                      >
                        {option.label}
                      </button>
                    ))}
                  </div>
                  <span className={styles.hint}>
                    {editing.transport === 'stdio'
                      ? t('mcpTypeStdioHint')
                      : editing.transport === 'http'
                        ? t('mcpTypeHttpHint')
                        : t('mcpTypeSseHint')}
                  </span>
                </Field>
                <Field label={t('mcpTimeout')} error={null}>
                  <TextInput
                    mono
                    type="text"
                    inputMode="numeric"
                    value={editing.timeoutMs}
                    onChange={(next) => setEditing({ ...editing, timeoutMs: next })}
                    placeholder="30000"
                  />
                </Field>
                <Field label={t('mcpProtocolVersion')} error={null}>
                  <div className={styles.typeChips} role="radiogroup">
                    {PROTOCOL_VERSION_OPTIONS.map((option) => (
                      <button
                        key={option.id}
                        type="button"
                        role="radio"
                        aria-checked={editing.protocolVersion === option.id}
                        className={`${styles.typeChip}${editing.protocolVersion === option.id ? ` ${styles.active}` : ''}`}
                        onClick={() =>
                          setEditing({ ...editing, protocolVersion: option.id })
                        }
                      >
                        {option.label}
                      </button>
                    ))}
                  </div>
                </Field>
                {editing.transport === 'stdio' ? (
                  <>
                    <Field label={t('mcpCommand')} error={null}>
                      <TextInput
                        mono
                        value={editing.command}
                        onChange={(next) => setEditing({ ...editing, command: next })}
                        placeholder={t('mcpCommandPlaceholder')}
                      />
                    </Field>
                    <Field label={t('mcpArgs')} hint={t('mcpArgsHint')} error={null}>
                      <TextInput
                        mono
                        value={editing.args}
                        onChange={(next) => setEditing({ ...editing, args: next })}
                        placeholder={t('mcpArgsPlaceholder')}
                      />
                    </Field>
                    <details className={styles.envBlock}>
                      <summary className={styles.envLabel}>{t('mcpEnv')}</summary>
                      <p className={styles.hint}>{t('mcpEnvHint')}</p>
                      <textarea
                        className={styles.envTextarea}
                        rows={4}
                        value={editing.envText}
                        onChange={(event) =>
                          setEditing({ ...editing, envText: event.target.value })
                        }
                        spellCheck={false}
                        placeholder={t('mcpEnvPlaceholder')}
                      />
                    </details>
                    <Field label={t('mcpCwd')} hint={t('mcpCwdHint')} error={null}>
                      <TextInput
                        mono
                        value={editing.cwd}
                        onChange={(next) => setEditing({ ...editing, cwd: next })}
                        placeholder={t('mcpCwdPlaceholder')}
                      />
                    </Field>
                  </>
                ) : (
                  <>
                    <Field label={t('mcpUrl')} hint={t('mcpUrlHint')} error={null}>
                      <TextInput
                        mono
                        value={editing.url}
                        onChange={(next) => setEditing({ ...editing, url: next })}
                        placeholder={t('mcpUrlPlaceholder')}
                      />
                    </Field>
                    <details className={styles.envBlock}>
                      <summary className={styles.envLabel}>{t('mcpHeaders')}</summary>
                      <p className={styles.hint}>{t('mcpHeadersHint')}</p>
                      <textarea
                        className={styles.envTextarea}
                        rows={4}
                        value={editing.headersText}
                        onChange={(event) =>
                          setEditing({ ...editing, headersText: event.target.value })
                        }
                        spellCheck={false}
                        placeholder={t('mcpHeadersPlaceholder')}
                      />
                    </details>
                  </>
                )}
              </form>
            ) : (
              <div className={styles.modalBody}>
                <Field label={t('mcpJsonFull')} error={null}>
                  <textarea
                    className={styles.jsonTextarea}
                    rows={10}
                    value={editing.rawJson}
                    onChange={(event) =>
                      setEditing({ ...editing, rawJson: event.target.value })
                    }
                    spellCheck={false}
                  />
                </Field>
                <p className={styles.hint}>{t('mcpJsonHint')}</p>
              </div>
            )}

            {editing.error && (
              <p className={styles.error} role="alert">
                {editing.error}
              </p>
            )}

            <footer className={styles.modalActions}>
              <Button variant="primary" onClick={() => void submit()} disabled={busyId === 'form'}>
                {busyId === 'form' ? t('loading') : t('save')}
              </Button>
              <Button onClick={() => setEditing(null)}>{t('cancel')}</Button>
            </footer>
          </div>
        </div>
      )}

      {!data || data.servers.length === 0 ? (
        <p className={styles.empty}>{t('mcpNoServers')}</p>
      ) : (
        <ul className={styles.list}>
          {data.servers.map((server) => {
            const busy = busyId === server.id
            const open = expanded === server.id
            return (
              <li className={styles.card} key={server.id}>
                <div className={styles.cardHead}>
                  <button
                    type="button"
                    className={styles.cardTitle}
                    aria-expanded={open}
                    onClick={() => setExpanded(open ? null : server.id)}
                  >
                    <span className={styles.cardName}>{server.id}</span>
                    <span className={styles.cardCommand}>
                      {server.transport === 'stdio'
                        ? `${server.command} ${server.args.join(' ')}`.trim()
                        : (server.url ?? server.transport)}
                    </span>
                  </button>
                  <div className={styles.cardMeta}>
                    <Badge
                      tone={
                        server.status === 'connected'
                          ? 'ok'
                          : server.status === 'error'
                            ? 'err'
                            : 'neutral'
                      }
                    >
                      {server.status === 'connected'
                        ? t('mcpStatusConnected')
                        : server.status === 'error'
                          ? t('mcpStatusError')
                          : t('mcpStatusDisabled')}
                    </Badge>
                    <span className={styles.toolCount}>
                      {t('mcpToolCount', { count: server.tools.filter((x) => x.enabled).length })}
                    </span>
                  </div>
                </div>

                {server.error && <p className={styles.serverError}>{server.error}</p>}

                {open && (
                  <div className={styles.cardBody}>
                    {server.envKeys.length > 0 && (
                      <p className={styles.meta}>
                        {t('mcpEnvKeys')}: {server.envKeys.join('、')}
                      </p>
                    )}
                    {server.cwd && (
                      <p className={styles.meta}>
                        {t('mcpCwd')}: <code>{server.cwd}</code>
                      </p>
                    )}
                    {server.tools.length === 0 ? (
                      <p className={styles.empty}>{t('mcpNoTools')}</p>
                    ) : (
                      <ul className={styles.tools}>
                        {server.tools.map((tool) => (
                          <li className={styles.tool} key={tool.qualified}>
                            <label className={styles.toolToggle}>
                              <input
                                type="checkbox"
                                checked={tool.enabled}
                                disabled={busyId === `${server.id}/${tool.name}`}
                                onChange={(event) =>
                                  void toggleTool(server.id, tool.name, event.target.checked)
                                }
                              />
                              <span className={styles.toolName}>{tool.name}</span>
                            </label>
                            {tool.description && (
                              <span className={styles.toolDesc}>{tool.description}</span>
                            )}
                            {tool.disabledReason && (
                              <span className={styles.toolReason}>{tool.disabledReason}</span>
                            )}
                          </li>
                        ))}
                      </ul>
                    )}
                  </div>
                )}

                <div className={styles.actions}>
                  {server.enabled ? (
                    <Button
                      small
                      disabled={busy}
                      onClick={() => void runServerAction(server.id, 'disable')}
                    >
                      {t('mcpDisable')}
                    </Button>
                  ) : (
                    <Button
                      small
                      disabled={busy}
                      onClick={() => void runServerAction(server.id, 'enable')}
                    >
                      {t('mcpEnable')}
                    </Button>
                  )}
                  <Button
                    small
                    disabled={busy}
                    onClick={() => void runServerAction(server.id, 'reconnect')}
                  >
                    {busy ? t('loading') : t('mcpReconnect')}
                  </Button>
                  <Button small disabled={busy} onClick={() => startEdit(server)}>
                    {t('edit')}
                  </Button>
                  <Button
                    small
                    variant="danger"
                    disabled={busy}
                    onClick={() => setPendingDelete(server.id)}
                  >
                    {t('delete')}
                  </Button>
                </div>
              </li>
            )
          })}
        </ul>
      )}

      {pendingDelete && (
        <ConfirmDialog
          open
          title={t('mcpDeleteTitle')}
          desc={t('mcpDeleteMessage', { id: pendingDelete })}
          confirmLabel={t('delete')}
          danger
          onCancel={() => setPendingDelete(null)}
          onConfirm={() => void removeServer(pendingDelete)}
        />
      )}
    </section>
  )
}
