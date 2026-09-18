// 修复历史会话里被写坏的口径:把 `inputTokens` 从「含缓存的总 prompt」还原为
// 「未缓存输入」,让它符合 `TokenUsage` 的契约(`cache_read_tokens` 是子集)。
//
//   node scripts/fix-input-tokens.mjs            # 预演,只报告不改
//   node scripts/fix-input-tokens.mjs --apply    # 实际改写(先自动备份)
//   node scripts/fix-input-tokens.mjs --apply --home <dir>   # 指定数据目录
//
// # 背景
//
// OpenAI 系协议(含 DeepSeek 官方)的 `prompt_tokens` 是**含缓存的总量**,
// 但 `crates/llm/src/wire.rs` 的映射曾把它原样塞进 `input_tokens`,违反了下游
// 共同假设的"input 只含未缓存部分"。后果是前端命中率公式
// `cache / (input + cache)` 把缓存算了两遍,99% 显示成 49%。
//
// 修复后的代码不再产生坏数据,但**已经落盘的会话不会自己变好** —— 那些日志里
// `input` 仍是总量。本脚本就是给这些旧会话做数据迁移。
//
// # 判别与改写
//
// 是否要修,靠**样本内部的关系**判断,不靠时间戳(时间戳会因实例重启、时钟
// 偏差而不可靠):
//
//   cache <= input  →  旧口径(input 是总量)      → 改为 input - cache
//   cache >  input  →  新口径(input 已是未缓存)  → 不动
//
// 依据:`input` 是总量时缓存命中不可能超过它(命中是它的子集),所以
// `cache > input` 在旧口径下物理上不成立;反之在新口径下极常见(未缓存只是
// 这一步新增的尾巴)。判据**幂等**:重复运行不会把已修好的数据再减一次。
//
// `input - cache` 在旧口径下恰好等于"未缓存输入"(总量减去命中),是精确的
// 算术还原,不是估算。
//
// 同一个 step 的 usage 在两处各存一份(`assistant-chunk.chunk.usage` 与
// `assistant-message.usage`),**两处必须同步改**,否则实时流与刷新后读出两组
// 不同的数(前端 fold 两条路径分别读这两处)。实测两者逐字一致,所以同一套
// 判据对两处都成立。
//
// # 安全边界
//
// 只改写**能被 JSON 解析、且 type 属于已知 usage 宿主**的行;工具输出里恰好
// 出现 `"usage":{...}` 字面量的文本行一律跳过 —— 会话日志里存着大量工具输出
// (有人读过 wire.rs,输出里就有这些字段名),盲改会把用户数据改烂。
import { copyFileSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { homedir } from 'node:os'

const args = process.argv.slice(2)
const apply = args.includes('--apply')
const homeIndex = args.indexOf('--home')
const home = homeIndex >= 0 ? args[homeIndex + 1] : join(homedir(), '.denia')
const sessionsDir = join(home, 'sessions')

/** usage 的字段顺序在不同版本里可能不同,所以按字段名抓,不依赖顺序。 */
const USAGE_RE = /"usage":\s*\{[^{}]*\}/g
const INPUT_RE = /"inputTokens":\s*(-?\d+)/
const CACHE_RE = /"cacheReadTokens":\s*(-?\d+)/

/** 只有这两类事件的 usage 是我们写入的;其余(尤其 tool-result)是用户数据。 */
const USAGE_HOSTS = new Set(['assistant-chunk', 'assistant-message'])

/**
 * 改写一行里出现的所有 usage 块。
 *
 * 返回改写后的行与改动处数。判据见文件头:`cache > input` 说明已是新口径。
 */
function fixLine(line) {
  let changed = 0
  const next = line.replace(USAGE_RE, (block) => {
    const inputMatch = INPUT_RE.exec(block)
    if (!inputMatch) return block
    const cacheMatch = CACHE_RE.exec(block)
    const input = Number(inputMatch[1])
    const cache = cacheMatch ? Number(cacheMatch[1]) : 0
    if (!Number.isFinite(input) || !Number.isFinite(cache)) return block
    // cache > input:已是新口径;cache == 0:本就全未缓存,无需动。
    if (cache <= 0 || cache > input) return block
    changed += 1
    return block.replace(INPUT_RE, `"inputTokens":${input - cache}`)
  })
  return { next, changed }
}

/** 该行是不是我们该碰的 usage 宿主(解析失败一律不碰)。 */
function isUsageHost(line) {
  let parsed
  try {
    parsed = JSON.parse(line)
  } catch {
    return false
  }
  if (!parsed || typeof parsed !== 'object') return false
  if (!USAGE_HOSTS.has(parsed.type)) return false
  // assistant-chunk 的 usage 在 chunk 里;assistant-message 的直接在顶层。
  if (parsed.type === 'assistant-chunk') return parsed.chunk?.type === 'usage'
  return parsed.usage != null
}

if (!statSync(sessionsDir, { throwIfNoEntry: false })?.isDirectory()) {
  console.error(`找不到会话目录:${sessionsDir}`)
  process.exit(1)
}

console.log(`数据目录:${home}`)
console.log(apply ? '模式:实际改写(会先备份)\n' : '模式:预演(不改动;加 --apply 执行)\n')

let files = 0
let fixedFiles = 0
let fixedBlocks = 0
let skippedNew = 0
const report = []

for (const name of readdirSync(sessionsDir)) {
  const file = join(sessionsDir, name, 'session.jsonl')
  if (!statSync(file, { throwIfNoEntry: false })?.isFile()) continue
  files += 1

  const original = readFileSync(file, 'utf8')
  const lines = original.split('\n')
  let fileBlocks = 0
  let fileSkipped = 0

  const output = lines.map((line) => {
    if (!line.includes('"usage"')) return line
    if (!isUsageHost(line)) return line
    // 先统计"已是新口径"的样本,便于报告区分。
    for (const block of line.match(USAGE_RE) ?? []) {
      const i = INPUT_RE.exec(block)
      const c = CACHE_RE.exec(block)
      if (i && c && Number(c[1]) > 0 && Number(c[1]) > Number(i[1])) fileSkipped += 1
    }
    const { next, changed } = fixLine(line)
    fileBlocks += changed
    return next
  })

  if (fileBlocks > 0) {
    fixedFiles += 1
    fixedBlocks += fileBlocks
    report.push({ name, blocks: fileBlocks })
    if (apply) {
      copyFileSync(file, `${file}.bak`)
      writeFileSync(file, output.join('\n'), 'utf8')
    }
  }
  skippedNew += fileSkipped
}

report.sort((a, b) => b.blocks - a.blocks)
if (report.length > 0) {
  console.log('需要修复的会话:')
  for (const item of report) console.log(`  ${item.name.slice(0, 8)}  ${item.blocks} 处`)
  console.log('')
}

console.log(`扫描会话 ${files} 个`)
console.log(`需修复 ${fixedFiles} 个,共 ${fixedBlocks} 处 inputTokens`)
console.log(`已是新口径、跳过 ${skippedNew} 处`)
if (!apply && fixedBlocks > 0) {
  console.log('\n(预演结束,未改动任何文件;加 --apply 执行)')
}
if (apply && fixedBlocks > 0) {
  console.log('\n已改写,原文件备份为 session.jsonl.bak')
}
