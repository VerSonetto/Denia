/** LLM 设置面板共享类型(与后端 OpenAiProfile / LlmFailure schema 对齐)。 */

import type {
  CatalogModel,
  OpenAiProfile,
  WireProtocol,
} from '../../types'

/** 一条已配置的网关路由(settings llm-openai providers map 的键值对)。 */
export interface ProviderRoute {
  route: string
  profile: OpenAiProfile
}

/* ---- 新增提供方模板(三步流程第一步) ---- */

export interface ProviderTemplate {
  id: 'openai' | 'deepseek' | 'anthropic'
  protocol: WireProtocol
  /** 模板预填的 baseURL(用户可改)。 */
  baseURL: string
  /** 推荐默认上下文窗口。 */
  contextWindow?: number
  /** i18n 键:模板名与一句话说明。 */
  labelKey: 'llm.template.openai' | 'llm.template.deepseek' | 'llm.template.anthropic'
  hintKey:
    | 'llm.template.openaiHint'
    | 'llm.template.deepseekHint'
    | 'llm.template.anthropicHint'
}

export const PROVIDER_TEMPLATES: readonly ProviderTemplate[] = [
  {
    id: 'openai',
    protocol: 'openai-completions',
    baseURL: '',
    labelKey: 'llm.template.openai',
    hintKey: 'llm.template.openaiHint',
  },
  {
    id: 'deepseek',
    protocol: 'openai-completions',
    baseURL: 'https://api.deepseek.com',
    contextWindow: 131_072,
    labelKey: 'llm.template.deepseek',
    hintKey: 'llm.template.deepseekHint',
  },
  {
    id: 'anthropic',
    protocol: 'anthropic-messages',
    baseURL: 'https://api.anthropic.com',
    contextWindow: 200_000,
    labelKey: 'llm.template.anthropic',
    hintKey: 'llm.template.anthropicHint',
  },
]

/* ---- 模型发现(DiscoverFlow) ---- */

export type DiscoverPhase = 'idle' | 'running' | 'done' | 'error'

export interface DiscoverState {
  phase: DiscoverPhase
  models: { id: string; name?: string }[]
  error: string | null
  /** 上次失败的本地时间戳(ms);成功或重新开始时清空。 */
  failedAt: number | null
}

/* ---- 连通性测试(ChatProbe) ---- */

export type ProbePhase = 'idle' | 'running' | 'success' | 'error'

export interface ProbeMonitor {
  /** performance.now() 起点;running 时有效。 */
  startedAt: number
  /** 响应头已返回(连接 OK)。 */
  connected: boolean
  /** 首 token 相对耗时(ms);未到为 null。 */
  ttftMs: number | null
  /** 已接收的输出字符数(驱动流式进度)。 */
  chars: number
  /** usage 上报(可能缺省)。 */
  usage: { input: number; output: number } | null
  /** 结束原因 id(stop/tool-calls/max-tokens…)。 */
  finish: string | null
}

export interface ProbeFailureView {
  code: string
  status?: number
  message: string
  requestId?: string
  retryAfterMs?: number
  /** 失败发生的本地时间戳(ms)。 */
  at: number
}

/* ---- 模型目录(ModelsPanel) ---- */

/** 目录平铺行:模型连同所属提供方。 */
export interface CatalogRow {
  providerId: string
  providerName: string
  model: CatalogModel
}

export type CatalogSortKey = 'provider' | 'id' | 'context'
export type CatalogSortDir = 'asc' | 'desc'

export interface CatalogFilter {
  query: string
  visionOnly: boolean
  thinkingOnly: boolean
  providerId: string | 'all'
}

/** 目录加载失败时的失败项(与 ModelCatalogFailure 对齐)。 */
export interface CatalogFailure {
  id: string
  name: string
  message: string
}
