import { useCallback, useEffect, useRef, useState } from 'react'
import * as api from '../api'
import { hasOpenTurn } from '../fold'
import type { SessionEnvelope, SessionHeader } from '../types'

/**
 * 会话事件流总线:每个会话一条流,单点负责快照、follow、seq 去重与
 * gap 自愈。视图只注册/注销监听,不持有流生命周期。
 *
 * 保活规则:只要还有监听者,流就活着;监听者清空但 turn 仍在运行时,
 * 流转入后台只维护 running 状态,收到 turn-end 后自动关闭——运行中
 * 切走会话,侧栏的运行圆点不再丢失。
 */

export interface StreamListener {
  /** 全量快照(重建 fold 用);gap 自愈后也会再次送达。 */
  onSnapshot(header: SessionHeader, events: SessionEnvelope[]): void
  /** 增量帧,已按 seq 去重并保证连续;gap 由总线自行修复。 */
  onEnvelope(envelope: SessionEnvelope): void
}

interface StreamState {
  controller: AbortController | null
  cursor: number
  listeners: Set<StreamListener>
  running: boolean
  lastResnapshotAt: number
}

const RESNAPSHOT_MIN_INTERVAL = 300

export function useSessionStreams() {
  const [runningIds, setRunningIds] = useState<Record<string, boolean>>({})
  const streams = useRef(new Map<string, StreamState>())

  const closeStream = useCallback((id: string, state: StreamState) => {
    state.controller?.abort()
    state.controller = null
    streams.current.delete(id)
  }, [])

  const setRunning = useCallback(
    (id: string, running: boolean, state: StreamState) => {
      if (state.running === running) return
      state.running = running
      setRunningIds((previous) => {
        if (previous[id] === running) return previous
        const next = { ...previous }
        if (running) next[id] = true
        else delete next[id]
        return next
      })
      // 轮次结束且无人订阅:流使命完成,关闭。
      if (!running && state.listeners.size === 0) closeStream(id, state)
    },
    [closeStream],
  )

  const openFollow = useCallback(
    (id: string, state: StreamState) => {
      state.controller = api.followSession(
        id,
        state.cursor,
        (envelope) => {
          if (envelope.seq <= state.cursor) return
          if (envelope.seq === state.cursor + 1) {
            state.cursor = envelope.seq
            if (envelope.type === 'turn-start') setRunning(id, true, state)
            else if (envelope.type === 'turn-end') setRunning(id, false, state)
            for (const listener of state.listeners) listener.onEnvelope(envelope)
          } else {
            // 帧断档(follow lag 会静默丢帧):重拿快照自愈。
            resnapshot(id, state)
          }
        },
        () => resnapshot(id, state),
      )
    },
    [setRunning],
  )

  const resnapshot = useCallback(
    (id: string, state: StreamState) => {
      const now = Date.now()
      if (now - state.lastResnapshotAt < RESNAPSHOT_MIN_INTERVAL) return
      state.lastResnapshotAt = now
      state.controller?.abort()
      void (async () => {
        try {
          const data = await api.getSession(id)
          state.cursor = data.events.length
            ? data.events[data.events.length - 1].seq
            : 0
          const running = hasOpenTurn(data.events)
          for (const listener of state.listeners) listener.onSnapshot(data.header, data.events)
          setRunning(id, running, state)
          if (state.listeners.size === 0 && !state.running) {
            closeStream(id, state)
            return
          }
          openFollow(id, state)
        } catch {
          // 会话被删/网络错误:保持关闭,下次 attach 再试。
          if (state.listeners.size === 0) closeStream(id, state)
        }
      })()
    },
    [closeStream, openFollow, setRunning],
  )

  /** 注册监听并取一次全量快照;返回注销函数。 */
  const attach = useCallback(
    (id: string, listener: StreamListener): (() => void) => {
      let state = streams.current.get(id)
      if (!state) {
        state = {
          controller: null,
          cursor: 0,
          listeners: new Set(),
          running: false,
          lastResnapshotAt: 0,
        }
        streams.current.set(id, state)
      }
      state.listeners.add(listener)
      resnapshot(id, state)
      return () => {
        const current = streams.current.get(id)
        if (!current) return
        current.listeners.delete(listener)
        if (current.listeners.size === 0 && !current.running) {
          closeStream(id, current)
        }
      }
    },
    [closeStream, resnapshot],
  )

  // 全量清理:App 卸载时关闭所有流。
  useEffect(() => {
    const all = streams.current
    return () => {
      for (const state of all.values()) state.controller?.abort()
      all.clear()
    }
  }, [])

  return { attach, runningIds }
}