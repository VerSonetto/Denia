/**
 * 流式渲染的物理内核:二阶阻尼弹簧、预测预留、闭环背压、字符积分揭示。
 *
 * 全部是纯函数,没有 DOM 与 React 依赖 —— 参数与参考实现(dsh-smooth-stream)
 * 保持一致,便于对着数字调手感,也便于在没有布局引擎的 jsdom 里钉死行为。
 *
 * ## 座标系
 * 所有"位置"都是滚动容器里的 px:目标(target)是内容底部的落点,位置(position)
 * 是视口当前所在的底。两者之差是 **滞后(lag)**:内容已经长出来、视口还没跟到。
 * 滞后不是缺陷,而是要留给读者的呼吸空间 —— 前提是它被限在一个容量里。
 */

/* ---- 弹簧跟随 ---- */

/** 刚度、阻尼、质量:欠阻尼前一刻的临界阻尼(k=130, c=24, m=1)。 */
export const SPRING_STIFFNESS = 130
export const SPRING_DAMPING = 24
export const SPRING_MASS = 1
/** 每 32ms 切片内的亚步进数:高速输出下欧拉积分也不发散。 */
export const SPRING_SUBSTEPS = 4
/**
 * 单帧物理时间上限(ms)。
 *
 * 主线程卡顿 300ms 之后,若按真实间隔积分,一帧就要追平几百 px —— 画面恢复
 * 的瞬间是瞬移。按 32ms 计物理时间,剩下的距离交给后面几帧平滑追上。
 */
export const MAX_FRAME_MS = 32
/** 认为"已经在目标上"的容差(px):亚像素残差不再写 DOM。 */
export const SPRING_REST_PX = 0.25
/**
 * 落地容差(px):剩余不到这么多时一次写完。
 *
 * 弹簧是渐近的:剩下 1~2px 时每帧只推进零点几 px,要走上百帧才落地。这段
 * 位移肉眼看不见,却会让"到底了没"这类几何判据(回底按钮、分页窗口、读者
 * 的回底动作)一直差一点点。3px 远小于一次折行(24~28px),落地不可见。
 */
export const SPRING_LAND_PX = 3

export interface SpringStep {
  /** 本帧推进的像素(0 ≤ advance ≤ lag)。 */
  advancePx: number
  /** 带进下一帧的速度(px/s),只增不减地收敛,不回弹。 */
  velocityPxPerSec: number
}

/**
 * 半隐式欧拉积分一步弹簧。
 *
 * 速度钳在非负:`lag` 恒为目标减位置,一旦越过目标(内容收缩)就回到 0,
 * 不会出现"追过头再弹回来"的回弹 —— 那是读者最敏感的一种廉价感。
 */
export function springStep(dtMs: number, lagPx: number, velocityPxPerSec = 0): SpringStep {
  if (!(lagPx > SPRING_REST_PX) || !(dtMs > 0)) return { advancePx: 0, velocityPxPerSec: 0 }
  // 最后不到 1px:直接落地,不让渐近尾巴拖住几何判据。
  if (lagPx <= SPRING_LAND_PX) return { advancePx: lagPx, velocityPxPerSec: 0 }
  const elapsedMs = Math.min(MAX_FRAME_MS, dtMs)
  const slices = Math.max(1, Math.ceil(elapsedMs / MAX_FRAME_MS))
  const subDt = elapsedMs / 1000 / slices / SPRING_SUBSTEPS
  let lag = lagPx
  let velocity = Math.max(0, velocityPxPerSec)
  for (let slice = 0; slice < slices; slice += 1) {
    for (let substep = 0; substep < SPRING_SUBSTEPS; substep += 1) {
      const acceleration = (SPRING_STIFFNESS * lag - SPRING_DAMPING * velocity) / SPRING_MASS
      velocity = Math.max(0, velocity + acceleration * subDt)
      const advance = velocity * subDt
      if (advance >= lag) return { advancePx: lagPx, velocityPxPerSec: 0 }
      lag -= advance
    }
  }
  return { advancePx: lagPx - lag, velocityPxPerSec: velocity }
}

/* ---- 预测预留 ---- */

/** 预留下限(px):一行折行的落点。 */
export const RESERVE_MIN_PX = 24
/**
 * 预留上限(px):高速输出时视口最多落后内容两行。
 *
 * 这个值同时受一个外部约束:对话视图把 `floor - scrollTop < 64` 当作"读者还在
 * 底部"(见 SessionView 的分页窗口裁剪)。预留超过 64 就会让流式期被判成"读者
 * 已经上翻",所以要留在 64 以内。
 */
export const RESERVE_MAX_PX = 56
/** 揭示速度低于这个值视为空闲,不再预留。 */
export const RESERVE_IDLE_CPS = 20
/** 揭示速度到这个值就吃满预留。 */
export const RESERVE_FULL_CPS = 600
/** 预留开合的时间常数(ms):跟着揭示速度走,但不能跳。 */
export const RESERVE_RESPONSE_MS = 180

/**
 * 当前揭示速度下该预留多少落底空间。
 *
 * 预留是给"折行"准备的:内容一行的最后几个字落定时,底部会突然多出
 * 一整行;如果视口此刻正贴着底,那一行就会以 24~28px 的一跳挤进视野。
 * 预留把这空间提前让出来,视口稳态停在离底 `reserve` px 处,折行落进
 * 预留里 —— 读者看到的是一条线在往上走,而不是整块跳一下。
 */
export function reserveTargetFor(speedCps: number): number {
  if (!(speedCps > RESERVE_IDLE_CPS)) return 0
  const normalized = Math.min(1, Math.max(0,
    (speedCps - RESERVE_IDLE_CPS) / (RESERVE_FULL_CPS - RESERVE_IDLE_CPS)))
  return RESERVE_MIN_PX + normalized * (RESERVE_MAX_PX - RESERVE_MIN_PX)
}

/** 预留的一阶跟随:180ms 时间常数,开与合都不会跳。 */
export function easeReserve(currentPx: number, targetPx: number, dtMs: number): number {
  if (!(dtMs > 0)) return currentPx
  const k = 1 - Math.exp(-Math.min(MAX_FRAME_MS, dtMs) / RESERVE_RESPONSE_MS)
  const next = currentPx + (targetPx - currentPx) * k
  return Math.abs(targetPx - next) <= SPRING_REST_PX ? targetPx : next
}

/* ---- 闭环背压 ---- */

/** 占满这个比例之前不减速。 */
export const BACKPRESSURE_START_RATIO = 0.1
/** 占满这个比例时减速到下限。 */
export const BACKPRESSURE_FULL_RATIO = 0.75
/** 揭示速度的下限倍数:再慢会让人以为卡住了。 */
export const BACKPRESSURE_MIN_SCALE = 0.55
/** 一行的估算高度(px):背压容量按"预留 + 一行"算。 */
export const LINE_HEIGHT_PX = 26

/**
 * 滞后占满容量时,给揭示引擎的减速系数(1 → 0.55)。
 *
 * 这是两个状态机之间唯一的耦合:跟随跟不上,就让揭示慢下来,而不是让
 * 读者看着尾巴滑进视野外。平滑插值(而不是阈值开关)避免在每次折行
 * 前后反复切换速度带来的节奏抖动。
 */
export function revealScaleFor(lagPx: number, capacityPx: number): number {
  if (!Number.isFinite(capacityPx) || capacityPx <= 0) {
    return lagPx > 0 ? BACKPRESSURE_MIN_SCALE : 1
  }
  const ratio = Math.min(1, Math.max(0, lagPx / capacityPx))
  if (ratio >= BACKPRESSURE_FULL_RATIO) return BACKPRESSURE_MIN_SCALE
  const progress = Math.min(1, Math.max(0,
    (ratio - BACKPRESSURE_START_RATIO) / (BACKPRESSURE_FULL_RATIO - BACKPRESSURE_START_RATIO)))
  const eased = progress * progress * (3 - 2 * progress)
  return 1 - (1 - BACKPRESSURE_MIN_SCALE) * eased
}

/* ---- 字符积分揭示 ---- */

/** 揭示基准速度(码元/秒):积压很小时的恒定吐字节奏。 */
export const QUEUE_BASE_SPEED_CPS = 90
/** 积压的加速指数:1.25 次幂,积压越大追得越狠,但不是线性的"按比例"。 */
export const QUEUE_ACCEL_EXPONENT = 1.25
/** 压力系数:乘在积压项上。 */
export const QUEUE_PRESSURE_FACTOR = 0.85
/** 揭示速度上限(码元/秒)。 */
export const QUEUE_MAX_SPEED_CPS = 600
/** 允许显示落后的码元上限:超过就按 0.15s 内追平的速度补。 */
export const LIVE_LAG_CHAR_CEILING = 32
/** 越过上限后的追平窗口(秒)。 */
const CATCHUP_SECONDS = 0.15

export interface RevealStep {
  /** 本帧放出的码元数。 */
  chars: number
  /** 进位到下一帧的小数债务:分数级积分不丢零头。 */
  debt: number
  /** 本帧的揭示速度(码元/秒),跟随引擎用它算预留。 */
  speedCps: number
}

/**
 * 一帧的揭示量:速度 = 90 + 积压^1.25 × 0.85(封顶 600),再乘背压系数。
 *
 * 与"每次把剩余距离的固定比例补上"不同,这里的速度只与积压有关:积压小时
 * 是恒定的打字机节奏(不会越放越慢),积压大时超线性追赶,但被上限与背压
 * 双重钳住 —— 一次网络分块送来几百字,画面仍是一行行长出来。
 */
export function queueRevealStep(
  backlog: number,
  dtMs: number,
  debt: number,
  scale = 1,
): RevealStep {
  if (!(backlog > 0) || !(dtMs > 0)) return { chars: 0, debt: 0, speedCps: 0 }
  const base = Math.min(
    QUEUE_MAX_SPEED_CPS,
    QUEUE_BASE_SPEED_CPS + Math.pow(backlog, QUEUE_ACCEL_EXPONENT) * QUEUE_PRESSURE_FACTOR,
  )
  const overflow = Math.max(0, backlog - LIVE_LAG_CHAR_CEILING)
  const target = overflow > 0 ? base + overflow / CATCHUP_SECONDS : base
  const speedCps = Math.min(QUEUE_MAX_SPEED_CPS * 1.5, target)
  const accumulated = Math.max(0, debt) + speedCps * Math.min(1, Math.max(0.05, scale)) * (dtMs / 1000)
  const chars = Math.min(backlog, Math.floor(accumulated))
  return { chars, debt: chars >= backlog ? 0 : accumulated - chars, speedCps }
}

/** 收尾排空的时长窗口(ms):积压越多给的时间越长,但有上下限。 */
export const DRAIN_MIN_MS = 100
export const DRAIN_MAX_MS = 280
/** 收尾排空的速度上下限(码元/秒)。 */
export const DRAIN_MIN_CPS = 200
export const DRAIN_MAX_CPS = 800

/**
 * 生产端结束后把残余缓冲区排空的速度(码元/秒)。
 *
 * 结算那一刻通常还剩几十个码元(揭示本来就允许略落后)。直接跳到全文等于
 * 把这段文字一次砸下来;按"剩余量 / 100~280ms"的速度匀速排空,收尾是平的。
 */
export function drainCpsFor(backlog: number): number {
  if (!(backlog > 0)) return 0
  const windowMs = Math.min(DRAIN_MAX_MS, Math.max(DRAIN_MIN_MS, backlog * 8))
  return Math.min(DRAIN_MAX_CPS, Math.max(DRAIN_MIN_CPS, (backlog * 1000) / windowMs))
}
