// 端到端验证:隧道上「SSE 不透传 / 长轮询可用」这条修复的实际效果。
//
// 与前面几个 probe 脚本的区别:那是在**最小 Node 源服务**上隔离变量;这里跑
// denia 真实服务 + 真实鉴权,并同时对照两条通道:
//
//   /api/events            全局失效通知(SSE 与 poll 各跑一份)
//   /api/sessions/:id/follow  会话事件正文(SSE 与 poll 各跑一份)
//
// SSE 的帧**按类型分开统计**:心跳帧(type=hb)与业务事件帧是两回事 —— 混在一起
// 会把"心跳能过但事件被攒着"误判成"全都不过",而这两种情形的修法不同。
//
// 会话事件用 goal 接口造(set/clear 交替:落 command-run + goal 事件并推给
// followers),不碰模型、不花钱,也不动用户正在对话的会话。
//
//   node scripts/verify-tunnel-poll.mjs [分钟数,默认 1]
import { setTimeout as delay } from 'node:timers/promises'

const LOCAL = 'http://127.0.0.1:3601'
const MINUTES = Number(process.argv[2] ?? 1)
const POLL_HOLD_SEC = 20 // 比服务端 30 上限短,窗口内能多跑几轮
const t0 = Date.now()
const at = () => Date.now() - t0

async function localJson(path, init) {
  const response = await fetch(`${LOCAL}${path}`, init)
  const body = await response.json().catch(() => null)
  if (!response.ok) throw new Error(`${path} → ${response.status} ${JSON.stringify(body)}`)
  return body
}

function ticketOf(url) {
  return /[?&]ticket=([^&]+)/.exec(url ?? '')?.[1] ?? null
}

async function login(base, link) {
  const headers = { 'content-type': 'application/json' }
  let response = await fetch(`${base}/api/remote/exchange`, {
    method: 'POST',
    headers,
    body: JSON.stringify({ ticket: ticketOf(link.ticketUrl) }),
  })
  let body = await response.json().catch(() => null)
  if (body?.status === 'pin-required') {
    response = await fetch(`${base}/api/remote/pin`, {
      method: 'POST',
      headers,
      body: JSON.stringify({ challenge: body.challenge, pin: link.pin }),
    })
    body = await response.json().catch(() => null)
  }
  const cookie = (response.headers.getSetCookie?.() ?? []).map((c) => c.split(';')[0]).join('; ')
  if (!cookie) throw new Error(`换不到 cookie:${JSON.stringify(body)}`)
  return cookie
}

/** 从 SSE 响应体里逐帧取出 data 的 type;注释行单独计数。 */
async function readSse(url, cookie, deadline, stats) {
  try {
    const response = await fetch(url, {
      headers: { accept: 'text/event-stream', cookie },
      signal: AbortSignal.timeout(Math.max(1000, deadline - Date.now())),
    })
    stats.status = response.status
    const reader = response.body.getReader()
    const decoder = new TextDecoder()
    let buffer = ''
    while (Date.now() < deadline) {
      const { done, value } = await reader.read()
      if (done) break
      buffer += decoder.decode(value, { stream: true })
      let boundary
      while ((boundary = buffer.indexOf('\n\n')) >= 0) {
        const chunk = buffer.slice(0, boundary)
        buffer = buffer.slice(boundary + 2)
        const dataLines = chunk.split('\n').filter((line) => line.startsWith('data:'))
        if (!dataLines.length) {
          stats.comments += 1
          continue
        }
        for (const line of dataLines) {
          let type = 'unparsable'
          try {
            type = JSON.parse(line.slice(5).trim())?.type ?? 'no-type'
          } catch { /* 保留 unparsable */ }
          if (type === 'hb') stats.heartbeats.push(at())
          else stats.events.push({ at: at(), type })
        }
      }
    }
  } catch (error) {
    stats.error = error.name
  }
  return stats
}

const newSseStats = () => ({ status: 0, events: [], heartbeats: [], comments: 0, error: '' })

let sessionId = null
let deleted = false
let startedTunnel = false
async function cleanup() {
  if (!sessionId || deleted) return
  deleted = true
  try {
    await localJson(`/api/sessions/${encodeURIComponent(sessionId)}`, { method: 'DELETE' })
    console.log(`\n测量会话已删除:${sessionId.slice(0, 8)}…`)
  } catch (error) {
    console.log(`\n! 测量会话删除失败(需手工清理):${error.message}`)
  }
}

try {
  // 顺序要紧:通道没开时 ticket/refresh 直接 400("当前没有开启的远程连接")。
  let status = await localJson('/api/remote/status')
  if (!status.lan) {
    await localJson('/api/remote/lan/start', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' })
  }
  startedTunnel = !status.tunnel
  if (startedTunnel) {
    console.log('起一条隧道用于测量…')
    await localJson('/api/remote/tunnel/start', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' })
  }
  await localJson('/api/remote/ticket/refresh', { method: 'POST' })
  status = await localJson('/api/remote/status')

  // 隧道**无论新旧**都要先探到"边缘已热"再开始测量:
  //  - 新起的隧道:Cloudflare 边缘的 DNS/TLS 有秒级生效延迟,第一枪常拿
  //    ECONNRESET("socket disconnected before secure TLS");
  //  - 上一轮留下的隧道:进程可能已被回收或边缘冷却,同样会 reset。
  // 这跟要测的东西无关,不先等就会出现"测量失败"假象。
  if (!status.tunnel) throw new Error('隧道起不来,无法验证')
  const readyDeadline = Date.now() + 90_000
  for (;;) {
    try {
      const probe = await fetch(`${status.tunnel.url}/api/remote/status`, {
        headers: { cookie: 'none=1' },
        signal: AbortSignal.timeout(8000),
      })
      // 401 也算就绪:说明请求已经打到应用层并被远程门挡下,链路是通的。
      if (probe.status === 401 || probe.ok) break
    } catch {
      /* 还没热 */
    }
    if (Date.now() > readyDeadline) throw new Error('隧道 90 秒内没就绪(边缘未生效或 cloudflared 已退出)')
    await delay(2000)
  }
  console.log('隧道已就绪')

  const created = await localJson('/api/sessions', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ cwd: process.cwd() }),
  })
  sessionId = created?.id ?? created?.session?.id
  if (!sessionId) throw new Error(`创建测量会话失败:${JSON.stringify(created).slice(0, 160)}`)
  console.log(`测量会话 ${sessionId.slice(0, 8)}…`)

  const targets = [
    { label: 'lan', base: `http://${status.lan.address}:${status.lan.port}`, cookie: await login(`http://${status.lan.address}:${status.lan.port}`, status.lan.link) },
    { label: 'tunnel', base: status.tunnel.url, cookie: await login(status.tunnel.url, status.tunnel.link) },
  ]

  const WINDOW_MS = MINUTES * 60_000
  const deadline = Date.now() + WINDOW_MS
  let fired = 0
  let sessionEventsWritten = 0

  async function fireOnce() {
    fired += 1
    const cfg = await localJson('/api/settings/remote')
    await localJson('/api/settings/remote', {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        value: { tunnel: { transportProtocol: cfg.value.tunnel.transportProtocol } },
        expectedRevision: cfg.revision,
      }),
    })
    // 会话事件:用 goal 接口造(不碰模型、不花钱,但会真落盘并推给 followers)。
    // 不用 set/clear 交替:那要求每次都不失败,一次 409 就永久错位。每轮都
    // "先无脑 clear(无 goal 时报错,忽略)、再 set",恒落两条事件。
    const goalPath = `/api/sessions/${encodeURIComponent(sessionId)}/goal`
    await localJson(goalPath, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ action: 'clear' }),
    }).catch(() => {})
    await localJson(goalPath, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        action: 'set',
        objective: `测量探针 ${fired}`,
        echoText: `/goal 测量探针 ${fired}`,
      }),
    })
    sessionEventsWritten += 2 // command-run 回显 + goal 事件
  }

  const ping = setInterval(() => { void fireOnce().catch((e) => console.log('  ! 造事件失败:', e.message)) }, 5000)
  await fireOnce().catch((e) => console.log('  ! 首次造事件失败:', e.message))

  const results = {}
  await Promise.all([
    ...targets.map(async (t) => {
      results[`${t.label}:sse-global`] = await readSse(`${t.base}/api/events`, t.cookie, deadline, newSseStats())
    }),
    ...targets.map(async (t) => {
      results[`${t.label}:sse-follow`] = await readSse(
        `${t.base}/api/sessions/${encodeURIComponent(sessionId)}/follow?after=0`, t.cookie, deadline, newSseStats())
    }),
    ...targets.map(async (t) => {
      results[`${t.label}:poll-global`] = await watchGlobalPoll(t, deadline)
    }),
    ...targets.map(async (t) => {
      results[`${t.label}:poll-follow`] = await watchFollowPoll(t, deadline)
    }),
  ])
  clearInterval(ping)

  async function watchGlobalPoll(target, stopAt) {
    const marks = []
    let cursor = 0
    const types = new Map()
    while (Date.now() < stopAt) {
      const started = at()
      try {
        const response = await fetch(`${target.base}/api/events/poll?after=${cursor}&wait=${POLL_HOLD_SEC}`, {
          headers: { cookie: target.cookie },
          signal: AbortSignal.timeout((POLL_HOLD_SEC + 15) * 1000),
        })
        if (!response.ok) {
          marks.push({ at: started, error: response.status })
          await delay(1000)
          continue
        }
        const batch = await response.json()
        cursor = batch.seq
        for (const event of batch.events ?? []) {
          types.set(event.type, (types.get(event.type) ?? 0) + 1)
          marks.push({ at: at(), latency: at() - started, type: event.type })
        }
      } catch (error) {
        marks.push({ at: started, error: error.name })
        await delay(1000)
      }
    }
    return { marks, types, cursor }
  }

  async function watchFollowPoll(target, stopAt) {
    const marks = []
    let cursor = 0
    let total = 0
    while (Date.now() < stopAt) {
      const started = at()
      try {
        const response = await fetch(
          `${target.base}/api/sessions/${encodeURIComponent(sessionId)}/follow/poll?after=${cursor}&wait=${POLL_HOLD_SEC}`,
          { headers: { cookie: target.cookie }, signal: AbortSignal.timeout((POLL_HOLD_SEC + 15) * 1000) },
        )
        if (!response.ok) {
          marks.push({ at: started, error: response.status })
          await delay(1000)
          continue
        }
        const batch = await response.json()
        const envelopes = batch.envelopes ?? []
        if (envelopes.length) {
          cursor = envelopes[envelopes.length - 1].seq
          total += envelopes.length
          marks.push({ at: at(), latency: at() - started, count: envelopes.length })
        }
      } catch (error) {
        marks.push({ at: started, error: error.name })
        await delay(1000)
      }
    }
    return { marks, total, cursor }
  }

  console.log(`\n=== ${MINUTES} 分钟窗口(造事件 ${fired} 次 ≈ ${sessionEventsWritten} 条会话事件)===`)
  for (const target of targets) {
    const gSse = results[`${target.label}:sse-global`]
    const fSse = results[`${target.label}:sse-follow`]
    const gPoll = results[`${target.label}:poll-global`]
    const fPoll = results[`${target.label}:poll-follow`]
    console.log(`\n[${target.label}]`)
    const sseLine = (label, s) => `  SSE ${label.padEnd(7)}: status=${s.status} 业务帧=${s.events.length} 心跳帧=${s.heartbeats.length} 注释=${s.comments} ${s.error || ''}`
    console.log(sseLine('global', gSse))
    console.log(sseLine('follow', fSse))
    const got = gPoll.marks.filter((m) => m.type)
    const errs = gPoll.marks.filter((m) => m.error)
    console.log(`  poll global  : 收到 ${got.length} 个事件,游标→${gPoll.cursor},分布 ${JSON.stringify([...gPoll.types])}`)
    if (errs.length) console.log(`               失败 ${errs.length} 次 ${JSON.stringify(errs.slice(0, 2))}`)
    const fGot = fPoll.marks.filter((m) => m.count)
    console.log(`  poll follow  : 收到 ${fPoll.total} 条会话事件(${fGot.length} 批),游标→${fPoll.cursor}` +
      (fPoll.marks.some((m) => m.error) ? ` 失败 ${JSON.stringify(fPoll.marks.filter((m) => m.error).slice(0, 2))}` : ''))
    if (got.length) console.log(`  poll 单事件延迟: 中位 ${median(got.map((m) => m.latency))} ms,最大 ${Math.max(...got.map((m) => m.latency))} ms`)
    if (fGot.length) console.log(`  follow 批延迟  : 中位 ${median(fGot.map((m) => m.latency))} ms`)
  }

  console.log('\n=== 判定 ===')
  let failed = 0
  const check = (passed, label) => {
    console.log(`  ${passed ? 'ok   ' : 'FAIL '} ${label}`)
    if (!passed) failed += 1
  }
  const t = (name) => results[`tunnel:${name}`]
  const l = (name) => results[`lan:${name}`]
  const tunnelGlobalBusiness = t('sse-global').events.length
  const tunnelFollowBusiness = t('sse-follow').events.length
  check(tunnelGlobalBusiness === 0, `隧道 SSE 全局业务帧 = 0(实测 ${tunnelGlobalBusiness};修复前提成立)`)
  check(tunnelFollowBusiness === 0, `隧道 SSE follow 业务帧 = 0(实测 ${tunnelFollowBusiness})`)
  check(t('poll-global').types.size > 0, '隧道 poll 全局通道能拿到事件')
  check(t('poll-global').cursor > 0, `隧道 poll 游标推进(=${t('poll-global').cursor})`)
  check(t('poll-follow').total > 0, `隧道 poll 拿到真实会话事件 ${t('poll-follow').total} 条`)
  check(!t('poll-global').marks.some((m) => m.error), '隧道 poll 无失败轮')
  check(l('sse-global').events.length > 0, `局域网 SSE 业务帧正常(${l('sse-global').events.length} 帧)—— 没把可用链路弄坏`)
  check(l('sse-follow').events.length > 0, `局域网 SSE follow 正常(${l('sse-follow').events.length} 帧)`)
  check(l('poll-follow').total > 0, '局域网 poll 也通(降级路径不挑链路)')
  check(t('sse-global').heartbeats.length === 0 || tunnelGlobalBusiness === 0,
    `隧道心跳帧=${t('sse-global').heartbeats.length}(与业务帧分开统计)`)

  if (startedTunnel) {
    await localJson('/api/remote/tunnel/stop', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' })
    console.log('\n测量用的隧道已关闭')
  }
  await cleanup()
  process.exit(failed ? 1 : 0)
} catch (error) {
  await cleanup()
  console.error('测量失败:', error)
  process.exit(2)
}

function median(values) {
  if (!values.length) return 0
  const sorted = [...values].sort((a, b) => a - b)
  return sorted[Math.floor(sorted.length / 2)]
}
