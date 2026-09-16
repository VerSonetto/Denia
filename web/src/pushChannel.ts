/**
 * 推送链路的健康判定:SSE 不通时改用长轮询。
 *
 * ## 为什么需要
 *
 * 实测(2026-09-16,`scripts/probe-*.mjs`):经 cloudflared 快速隧道访问时,两条
 * SSE 通道(`/api/events` 与会话 follow)的响应头都拿得到 200,但**一帧 data 都
 * 透不过来** —— 首帧垫 2KB、垫 16KB、心跳提到 1 秒,四种变体全是 0 块;同一条
 * 隧道上挂 45 秒的普通 JSON 响应却完好穿透。
 *
 * 用户看到的症状是"AI 几秒就回完了,手机上几十秒才跳出来":follow 流收不到帧
 * → 32 秒无帧看门狗超时 → 重连 + 重拉全量快照(普通 GET 能过隧道)→ 内容每
 * ~32 秒对齐一次。
 *
 * ## 判据是"帧",不是"错误"
 *
 * 半死的链路不报错:握手正常、连接不断,只是没数据。EventSource 对这种通道会
 * 一直安静地等,`onerror` 都不触发。所以只能看**多久没收到任何一帧**。服务端
 * 为此把心跳发成带 data 的帧(15 秒一次)—— 注释行不被 EventSource 派发,拿它
 * 无法区分"链路活着只是没事件"与"链路已经死了"。
 *
 * ## 一条铁律:长轮询通 ≠ SSE 通
 *
 * 降级后轮询当然会成功(隧道上它就是能过)。如果拿轮询的成功当作"SSE 活性",
 * 就会立刻解除降级 → 回到 SSE → 又 0 帧 → 再判死 → 再降级……每轮白等十几秒,
 * 比不修还糟。所以 **只有 SSE 帧能证明 SSE 活着**(`noteSseFrame`),轮询一侧
 * 一律不上报。解除降级只能走显式重探(`reprobePush`),且带冷却时间。
 */

/** 心跳周期,与服务端 `api::events::HEARTBEAT_INTERVAL_SECS` 一致。 */
const HEARTBEAT_MS = 15_000

/** 运行中断连判定:两个心跳周期的抖动余量 + 移动网络尾部延迟。 */
const STALLED_MS = HEARTBEAT_MS * 2 + 5_000

/** 冷启动判定:健康链路的首帧是毫秒级,给一个心跳 + 3 秒已极宽。 */
const FIRST_FRAME_MS = HEARTBEAT_MS + 3_000

/**
 * 两次重探 SSE 的最小间隔。
 *
 * 没有它,用户每次切回前台都会白等一整个判定周期(手机上就是"回来又卡住了");
 * 判定成 SSE 不通之后,网络条件在分钟级尺度上不会突变。
 */
const REPROBE_COOLDOWN_MS = 5 * 60_000

/** 本轮 SSE 连接建立时刻;0 = 从未建立过(与"停滞"是两回事,见 isPushStalled)。 */
let attemptAt = 0
/** 最近一次收到 SSE 帧的时刻;0 = 本轮还没收到过。 */
let lastFrameAt = 0
let pushDead = false
let lastReprobeAt = 0

const watchers = new Set<(dead: boolean) => void>()

/** 本轮 SSE 连接开始:计时清零,开始等首帧。 */
export function markPushAttempt(now = Date.now()): void {
  attemptAt = now
  lastFrameAt = 0
}

/**
 * 收到一个 SSE 帧(含心跳)。这是 SSE 通道活着的唯一凭据。
 *
 * 降级期间还能收到帧,说明链路真的恢复了(例如换到局域网直连),立即解除降级。
 */
export function noteSseFrame(now = Date.now()): void {
  lastFrameAt = now
  if (pushDead) setPushDead(false)
}

/**
 * 当前 SSE 是否已经判死。
 *
 * 从未建立过连接时返回 false —— 初始态不是停滞。早先这里用 `now - 0` 参与
 * 比较,冷启动即被判成"停滞",导致第一次选传输就直接跳过 SSE。
 */
export function isPushStalled(now = Date.now()): boolean {
  if (!attemptAt) return false
  return lastFrameAt ? now - lastFrameAt >= STALLED_MS : now - attemptAt >= FIRST_FRAME_MS
}

export function isPushDead(): boolean {
  return pushDead
}

export function setPushDead(dead: boolean): void {
  if (pushDead === dead) return
  pushDead = dead
  if (dead) lastReprobeAt = Date.now()
  for (const watcher of watchers) watcher(dead)
}

/** 订阅降级状态变化,让两条通道(全局事件流 + 会话 follow 流)同步切换传输。 */
export function subscribePushDead(watcher: (dead: boolean) => void): () => void {
  watchers.add(watcher)
  return () => watchers.delete(watcher)
}

/**
 * 请求重探 SSE:回到前台、网络状态变化时调用。
 *
 * 手机在隧道 / 局域网 / 蜂窝之间来回切,降级不该是单向门。带冷却:刚判死不
 * 久就重探,只会让用户在"探测 → 白等一整个判定周期"里来回挨卡。
 *
 * @returns 是否应当真的重探(未被冷却拦下)。
 */
export function reprobePush(now = Date.now()): boolean {
  if (!pushDead) return true
  if (now - lastReprobeAt < REPROBE_COOLDOWN_MS) return false
  lastReprobeAt = now
  setPushDead(false)
  return true
}

/** 仅测试用:回到初始状态。 */
export function __resetPushHealth(): void {
  attemptAt = 0
  lastFrameAt = 0
  pushDead = false
  lastReprobeAt = 0
  watchers.clear()
}
