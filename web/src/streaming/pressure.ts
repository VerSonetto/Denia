/**
 * 两个状态机之间的唯一通道:揭示引擎上报速率,跟随引擎下发减速系数。
 *
 * 它是一份**可变对象**而不是 React state:两端都在 rAF 里逐帧读写,走 state
 * 等于每帧触发一次重渲染,而这里传递的只是两个数字。谁读谁写都发生在同一帧
 * 内(揭示引擎先读 scale、后写 speedCps),不需要订阅机制。
 */

import { createContext, useContext } from 'react'

export interface StreamPressure {
  /**
   * 揭示速率(码元/秒):由揭示引擎每次提交上报。
   *
   * 跟随引擎据此决定预留多少落底空间(见 `reserveTargetFor`)。停流后不再
   * 上报,超过 TTL 即视为空闲,预留随之收回。
   */
  speedCps: number
  /** 最近一次上报的时刻(`performance.now()`)。 */
  speedAt: number
  /**
   * 揭示减速系数(0.55~1):跟随引擎按滞后占容量的比例下发。
   *
   * 揭示引擎每帧读它乘进自己的速度里 —— 跟随跟不上时让内容慢下来,
   * 而不是让读者看着尾巴滑出视野。
   */
  scale: number
}

/** 新建一份压力通道(每个滚动容器一份)。 */
export function createPressure(): StreamPressure {
  return { speedCps: 0, speedAt: 0, scale: 1 }
}

/**
 * 上报速率超过这么久就视为空闲。
 *
 * 220ms ≈ 六七个提交间隔:偶发的一两次提交间隙不会让预留抖动收回。
 */
export const SPEED_TTL_MS = 220

/**
 * 对话流所在的滚动容器的压力通道。
 *
 * 揭示引擎(正文 / 思考正文)挂得很深,逐层透传一个 ref 会污染整条组件链,
 * 因此对话流走 context;自带内层滚动框的组件(思考框)则以显式参数覆盖。
 */
export const SmoothStreamContext = createContext<StreamPressure | null>(null)

/** 取当前对话流的压力通道;不在流式宿主下(预览、计划面板)返回 null。 */
export function useStreamPressure(): StreamPressure | null {
  return useContext(SmoothStreamContext)
}
