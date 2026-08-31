/**
 * Console copy. Chinese is the key-set source of truth; `en` is typed
 * against `zh`, so a missing or extra English key is a compile error.
 */

const zh = {
  appName: 'dsh-rs 控制台',
  navSessions: '会话',
  navModels: '模型配置',
  connectionLost: '与服务器的连接断开,正在重试…',

  newSession: '新建会话',
  sandboxLabel: '沙箱工作区',
  cwdLabel: '工作目录',
  cwdPlaceholder: '真实目录的绝对路径(关沙箱时生效)',
  emptySessions: '还没有会话。新建一个,开始和 agent 对话。',
  emptyTranscript: '发一条消息开始这个会话。',
  heroTitle: '和 agent 一起干活',
  heroSub: '新建一个会话,agent 会用 bash、read_file、write_file 在工作区里完成任务,整个过程透明可见。',
  thinkTitle: '思考',
  sessionDeleted: '会话已删除',
  deleteSession: '删除',
  runningLabel: '运行中',

  defaultModelTitle: '默认模型',
  defaultModelHint: '新会话使用的模型。选择会立即写入配置。',
  providerLabel: '提供方',
  modelLabel: '模型',
  reasoningLabel: '推理强度',
  providerDefault: '跟随提供方默认',
  saveDefaultModel: '应用',
  defaultModelSaved: '默认模型已更新',
  catalogFailure: '目录加载失败',
  noModels: '该提供方暂无可用模型',

  deepseekTitle: 'DeepSeek 官方',
  deepseekDescription: '通过 api.deepseek.com 调用 DeepSeek 模型。',
  apiKeyLabel: 'API Key',
  apiKeyPlaceholder: '输入密钥后保存',
  apiKeySet: '已配置',
  apiKeyMissing: '未配置',
  apiKeyEnvShadowed: '由环境变量提供',
  apiKeyFromEnv: '环境变量',
  apiKeyFromFile: '凭据文件',
  apiKeyFromProjectEnv: '项目 .env',
  apiKeyFromUserEnv: '用户 .env',
  saveKey: '保存密钥',
  removeKey: '移除',
  keySaved: '密钥已保存',
  keyRemoved: '密钥已移除',
  baseURLLabel: 'Base URL',
  saveProviderConfig: '保存配置',
  providerConfigSaved: '配置已保存',
  thinkingLabel: '思考模式',
  thinkingEnabled: '启用',
  thinkingDisabled: '禁用',

  customTitle: '自定义提供方',
  customDescription: '任何 OpenAI 兼容网关:自定义 Base URL、密钥与模型列表。',
  addProvider: '添加提供方',
  routeIdLabel: '路由 ID',
  routeIdPlaceholder: 'my-gateway',
  displayNameLabel: '显示名称',
  modelsLabel: '模型',
  discoverModels: '探测模型',
  discovering: '探测中…',
  discoveredCount: '探测到 {n} 个模型',
  discoverFailed: '探测失败',
  removeProvider: '删除',
  editProvider: '编辑',
  providerSaved: '提供方已保存',
  providerRemoved: '提供方已删除',
  routeIdInvalid: '路由 ID 只能包含小写字母、数字和连字符,且以字母开头',
  baseURLRequired: 'Base URL 不能为空',
  keyRefLabel: '密钥引用',

  chatPlaceholder: '输入消息…(Enter 发送,Shift+Enter 换行)',
  run: '发送',
  stop: '停止',
  reasoningBlock: '思考过程',
  interrupted: '已中断',
  toolResultLabel: '结果',
  usageLabel: '用量',
  inputTokens: '输入',
  outputTokens: '输出',
  cacheReadTokens: '缓存读',
  reasoningTokens: '推理',
  reasonCompleted: '完成',
  reasonAborted: '已中止',
  reasonMaxTokens: '达到长度上限',
  reasonError: '出错',
  currentModel: '默认模型',

  loading: '加载中…',
  retry: '重试',
  cancel: '取消',
  save: '保存',
  close: '关闭',
  error: '出错了',
}

const en: typeof zh = {
  appName: 'dsh-rs console',
  navSessions: 'Sessions',
  navModels: 'Models',
  connectionLost: 'Lost connection to the server, retrying…',

  newSession: 'New session',
  sandboxLabel: 'Sandbox workspace',
  cwdLabel: 'Working directory',
  cwdPlaceholder: 'Absolute path to a real directory (when sandbox is off)',
  emptySessions: 'No sessions yet. Create one and start talking to the agent.',
  emptyTranscript: 'Send a message to start this session.',
  heroTitle: 'Work alongside your agent',
  heroSub: 'Create a session. The agent gets bash, read_file and write_file in its workspace, and every step stays visible.',
  thinkTitle: 'Think',
  sessionDeleted: 'Session deleted',
  deleteSession: 'Delete',
  runningLabel: 'running',

  defaultModelTitle: 'Default model',
  defaultModelHint: 'Used by new sessions. Saved immediately.',
  providerLabel: 'Provider',
  modelLabel: 'Model',
  reasoningLabel: 'Reasoning effort',
  providerDefault: 'Provider default',
  saveDefaultModel: 'Apply',
  defaultModelSaved: 'Default model updated',
  catalogFailure: 'Catalog failed to load',
  noModels: 'No models available from this provider',

  deepseekTitle: 'DeepSeek official',
  deepseekDescription: 'Calls DeepSeek models through api.deepseek.com.',
  apiKeyLabel: 'API key',
  apiKeyPlaceholder: 'Enter the key, then save',
  apiKeySet: 'Configured',
  apiKeyMissing: 'Not configured',
  apiKeyEnvShadowed: 'Provided by environment',
  apiKeyFromEnv: 'process env',
  apiKeyFromFile: 'credentials file',
  apiKeyFromProjectEnv: 'project .env',
  apiKeyFromUserEnv: 'user .env',
  saveKey: 'Save key',
  removeKey: 'Remove',
  keySaved: 'Key saved',
  keyRemoved: 'Key removed',
  baseURLLabel: 'Base URL',
  saveProviderConfig: 'Save configuration',
  providerConfigSaved: 'Configuration saved',
  thinkingLabel: 'Thinking mode',
  thinkingEnabled: 'Enabled',
  thinkingDisabled: 'Disabled',

  customTitle: 'Custom providers',
  customDescription: 'Any OpenAI-compatible gateway: base URL, key, and model list.',
  addProvider: 'Add provider',
  routeIdLabel: 'Route ID',
  routeIdPlaceholder: 'my-gateway',
  displayNameLabel: 'Display name',
  modelsLabel: 'Models',
  discoverModels: 'Discover models',
  discovering: 'Discovering…',
  discoveredCount: 'Discovered {n} models',
  discoverFailed: 'Discovery failed',
  removeProvider: 'Remove',
  editProvider: 'Edit',
  providerSaved: 'Provider saved',
  providerRemoved: 'Provider removed',
  routeIdInvalid: 'Route IDs are lowercase letters, digits, hyphens, led by a letter',
  baseURLRequired: 'Base URL is required',
  keyRefLabel: 'Key reference',

  chatPlaceholder: 'Type a message… (Enter to send, Shift+Enter for newline)',
  run: 'Send',
  stop: 'Stop',
  reasoningBlock: 'Reasoning',
  interrupted: 'interrupted',
  toolResultLabel: 'result',
  usageLabel: 'Usage',
  inputTokens: 'input',
  outputTokens: 'output',
  cacheReadTokens: 'cache read',
  reasoningTokens: 'reasoning',
  reasonCompleted: 'Completed',
  reasonAborted: 'Aborted',
  reasonMaxTokens: 'Hit the token limit',
  reasonError: 'Error',
  currentModel: 'Default model',

  loading: 'Loading…',
  retry: 'Retry',
  cancel: 'Cancel',
  save: 'Save',
  close: 'Close',
  error: 'Something went wrong',
}

type LocaleId = 'zh' | 'en'

const dictionaries: Record<LocaleId, typeof zh> = { zh, en }

function detectLocale(): LocaleId {
  if (typeof navigator !== 'undefined' && navigator.language?.toLowerCase().startsWith('en')) {
    return 'en'
  }
  return 'zh'
}

const active: LocaleId = detectLocale()

/** Translates one key, interpolating `{name}` params. */
export function t(key: keyof typeof zh, params?: Record<string, string | number>): string {
  let text = dictionaries[active][key]
  if (params) {
    for (const [name, value] of Object.entries(params)) {
      text = text.replaceAll(`{${name}}`, String(value))
    }
  }
  return text
}
