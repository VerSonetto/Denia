/**
 * `llm-openai` providers 的自动保存通道 —— 模型面板唯一的写入口。
 *
 * 设置存储是「整段命名空间替换 + 乐观并发」,所以自动保存必须解决四件事:
 * 1. 基线:全表与 revision 由本通道自己维护(写入响应带回新 revision),不拿弹窗
 *    打开时的快照 —— 否则第二次改动必然 409。
 * 2. 防覆盖:没拿到服务端基线前绝不写。整表替换在空基线上会把别人的网关一起清空。
 * 3. 串行:同一时刻只有一个 PUT 在途;同一路由排队中的改动合并成最后一条。
 * 4. 自愈:409 时重拉最新全表、把本次改动重新套用后重试一次;仍失败才报错。
 *
 * 对外暴露的回调身份稳定,调用方可以直接放进 useEffect 依赖里。
 */

import { useCallback, useRef, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { asRecord, namespaceOf, OPENAI_NS } from './providerSettings'
import type { Notify } from '../../App'

export type AutosaveStatus = 'idle' | 'saving' | 'saved' | 'error'

/** 一条待落盘的改动;profile 为 null 表示删除该路由。 */
interface QueueItem {
  route: string
  profile: Record<string, unknown> | null
  /** 这一条改动的等待者;同路由合并后一起结算,不会说谎报成功。 */
  resolvers: Array<(ok: boolean) => void>
}

export interface ProviderAutosave {
  status: AutosaveStatus
  /** 最近一次成功落盘的本地时间戳(ms)。 */
  savedAt: number | null
  /** 最近一次失败原因;成功即清空。 */
  error: string | null
  /** 本面板新建/编辑过的路由,最近优先;卡片列表据此置顶。 */
  recent: string[]
  /** 用服务端快照打底(首拉与每次刷新都调用)。 */
  prime: (providers: Record<string, unknown>, revision: number) => void
  /** 该路由当前是否已存在(以通道持有的全表为准)。 */
  has: (route: string) => boolean
  /** 排队落盘一个路由;null = 删除。resolve 这条改动是否成功。 */
  write: (route: string, profile: Record<string, unknown> | null) => Promise<boolean>
  /** 等所有在途与排队的写入落定;返回是否全部成功。 */
  flush: () => Promise<boolean>
  /** 重放最后一条失败的改动。 */
  retry: () => Promise<boolean>
}

/** 按 recent 置顶排序;未触及过的路由保持原键序。纯函数,便于单独验证。 */
export function sortRoutesByRecent<T extends { route: string }>(
  routes: T[],
  recent: readonly string[],
): T[] {
  if (recent.length === 0) return routes
  const rank = new Map(recent.map((route, index) => [route, index]))
  return [...routes].sort((a, b) => {
    const av = rank.get(a.route)
    const bv = rank.get(b.route)
    if (av === undefined && bv === undefined) return 0
    if (av === undefined) return 1
    if (bv === undefined) return -1
    return av - bv
  })
}

/**
 * 与键顺序无关的深比较。两侧都先剔掉值为 undefined 的键 —— 表单构造出的
 * profile 会带 `displayName: undefined` 这类占位键,服务端回来的对象没有,
 * 不剔除就会被判成"内容变了",导致每次打开编辑弹窗都空写一次。
 */
export function profileEquals(a: unknown, b: unknown): boolean {
  if (a === b) return true
  if (Array.isArray(a) || Array.isArray(b)) {
    if (!Array.isArray(a) || !Array.isArray(b) || a.length !== b.length) return false
    return a.every((item, index) => profileEquals(item, b[index]))
  }
  if (isPlainObject(a) && isPlainObject(b)) {
    const ak = definedKeys(a)
    const bk = definedKeys(b)
    if (ak.length !== bk.length) return false
    return ak.every((key) => key in b && profileEquals(a[key], b[key]))
  }
  return false
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function definedKeys(value: Record<string, unknown>): string[] {
  return Object.keys(value).filter((key) => value[key] !== undefined)
}

export function useProviderAutosave({
  notify,
  onChanged,
}: {
  notify: Notify
  onChanged: () => void
}): ProviderAutosave {
  const [status, setStatus] = useState<AutosaveStatus>('idle')
  const [savedAt, setSavedAt] = useState<number | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [recent, setRecent] = useState<string[]>([])

  // 这两个每次都变,收进 ref,免得回调身份跟着渲染飘。
  const notifyRef = useRef(notify)
  notifyRef.current = notify
  const changedRef = useRef(onChanged)
  changedRef.current = onChanged

  /** 本通道认定的 providers 全表(服务端快照 + 自己的写入)。 */
  const providersRef = useRef<Record<string, unknown>>({})
  const revisionRef = useRef(0)
  const primedRef = useRef(false)
  /** 本面板写过的路由:null 表示「刻意删除」,压过迟到的外部快照。 */
  const ownedRef = useRef<Map<string, Record<string, unknown> | null>>(new Map())
  const queueRef = useRef<QueueItem[]>([])
  const runningRef = useRef(false)
  const chainOkRef = useRef(true)
  const settleChainRef = useRef<((ok: boolean) => void) | null>(null)
  const allSettledRef = useRef<Promise<boolean>>(Promise.resolve(true))
  const failedRef = useRef<QueueItem | null>(null)

  const markRecent = useCallback((route: string, alive: boolean) => {
    setRecent((prev) => {
      const rest = prev.filter((entry) => entry !== route)
      return alive ? [route, ...rest] : rest
    })
  }, [])

  /** 把一条改动套进给定的全表,返回新表。 */
  const apply = useCallback((base: Record<string, unknown>, item: QueueItem) => {
    const next: Record<string, unknown> = { ...base }
    if (item.profile === null) delete next[item.route]
    else next[item.route] = item.profile
    return next
  }, [])

  const put = useCallback(
    async (providers: Record<string, unknown>, revision: number) => {
      const view = (await api.replaceNamespace(
        OPENAI_NS,
        { providers },
        revision,
      )) as { revision: number }
      return { providers, revision: view.revision }
    },
    [],
  )

  const commit = useCallback(
    async (item: QueueItem): Promise<boolean> => {
      setStatus('saving')
      setError(null)
      const succeed = (providers: Record<string, unknown>, revision: number) => {
        providersRef.current = providers
        revisionRef.current = revision
        ownedRef.current.set(item.route, item.profile)
        failedRef.current = null
        setStatus('saved')
        setSavedAt(Date.now())
        markRecent(item.route, item.profile !== null)
        changedRef.current()
      }
      const fail = (message: string) => {
        setError(message)
        setStatus('error')
        failedRef.current = item
        notifyRef.current('err', t('llm.autosave.error', { reason: message }))
      }

      try {
        const result = await put(apply(providersRef.current, item), revisionRef.current)
        succeed(result.providers, result.revision)
        return true
      } catch (err) {
        // 版本冲突说明这张表已被别处改动:拉最新全表,把本次改动重新套上去再试
        // 一次。其余错误(校验被拒、网络)不重放 —— 重放也是同样的结果。
        const conflict =
          err instanceof api.ApiError && (err.status === 409 || err.code === 'settings/conflict')
        if (!conflict) {
          fail(err instanceof Error ? err.message : String(err))
          return false
        }
        try {
          const settings = await api.getSettings()
          const view = namespaceOf(settings, OPENAI_NS)
          const merged = { ...providersRef.current, ...asRecord(view?.value?.providers) }
          const result = await put(apply(merged, item), view?.revision ?? 0)
          succeed(result.providers, result.revision)
          return true
        } catch (retryErr) {
          fail(retryErr instanceof Error ? retryErr.message : String(retryErr))
          return false
        }
      }
    },
    [apply, markRecent, put],
  )

  const drain = useCallback(async () => {
    let ok = true
    while (queueRef.current.length > 0) {
      const item = queueRef.current.shift() as QueueItem
      const committed = await commit(item)
      for (const resolve of item.resolvers) resolve(committed)
      ok = ok && committed
    }
    runningRef.current = false
    settleChainRef.current?.(chainOkRef.current && ok)
    chainOkRef.current = true
  }, [commit])

  /** 入队一条改动;同路由还在队列里时就地合并(只保留最新内容)。 */
  const enqueue = useCallback(
    (route: string, profile: Record<string, unknown> | null): Promise<boolean> => {
      const waiter = new Promise<boolean>((resolve) => {
        const pending = queueRef.current.find((item) => item.route === route)
        if (pending) {
          pending.profile = profile
          pending.resolvers.push(resolve)
          return
        }
        queueRef.current.push({ route, profile, resolvers: [resolve] })
      })
      if (!runningRef.current) {
        runningRef.current = true
        chainOkRef.current = true
        allSettledRef.current = new Promise<boolean>((resolve) => {
          settleChainRef.current = resolve
        })
        void drain()
      }
      return waiter
    },
    [drain],
  )

  const prime = useCallback((providers: Record<string, unknown>, revision: number) => {
    const merged = { ...providers }
    for (const [route, profile] of ownedRef.current) {
      if (profile === null) delete merged[route]
      else merged[route] = profile
    }
    providersRef.current = merged
    // 在途写入的 revision 由响应自己推进,外部快照不能把它回退掉。
    if (!runningRef.current) revisionRef.current = revision
    primedRef.current = true
  }, [])

  const has = useCallback((route: string) => route in providersRef.current, [])

  const write = useCallback(
    (route: string, profile: Record<string, unknown> | null): Promise<boolean> => {
      if (!primedRef.current) {
        // 基线未就绪:这时整表替换会连别人的网关一起清掉,宁可拒绝。
        notifyRef.current('err', t('llm.autosave.notReady'))
        return Promise.resolve(false)
      }
      // 内容没变就不写:光打开编辑弹窗再关掉不该 bump revision,也不该把
      // 那个网关顶到列表最前(置顶只该跟随真正的改动)。
      if (profile !== null && profileEquals(providersRef.current[route], profile)) {
        return Promise.resolve(true)
      }
      return enqueue(route, profile)
    },
    [enqueue],
  )

  const flush = useCallback(() => allSettledRef.current, [])

  const retry = useCallback(() => {
    const item = failedRef.current
    if (!item) return allSettledRef.current
    failedRef.current = null
    setError(null)
    return enqueue(item.route, item.profile)
  }, [enqueue])

  return { status, savedAt, error, recent, prime, has, write, flush, retry }
}
