// 构建期预压缩控制台产物:给每个文本文件旁挂 .br 与 .gz 实体变体。
//
// 为什么放在构建期而不是运行时压缩:经公网隧道发到手机上时**字节数就是延迟**
// (主包 1.5 MB 级,弱网上多跑几百 KB 就是几百毫秒)。构建期可以随便用
// brotli q11 换最小体积,运行时零 CPU、零缓冲;服务端只按 Accept-Encoding
// 选一个现成实体发出去(见 crates/server/src/web_assets.rs)。
//
// 位置:front-end 工具链目录(web/scripts),由 web/package.json 的 build 脚本
// 与仓库根的 scripts/desktop.ps1 调用。放在 web/ 内部才能让 `pnpm build`
// 不依赖仓库根的 scripts/ 目录。
//
// 跳过图片与字体:它们自身已压缩,再压一遍纯属浪费(woff2 内部已是 Brotli)。
//
// 并发:q11 是单核 CPU 密集操作,串行压 60+ 个文件要 10 s 级,占桌面端构建
// 总时长里最不划算的一段。这里按 CPU 数切分文件、每个 worker 线程独立压缩,
// 产物字节与串行完全一致(只并行、不换参数)。

import { brotliCompressSync, gzipSync, constants as zlibConstants } from 'node:zlib'
import { readdirSync, statSync, readFileSync, writeFileSync, rmSync } from 'node:fs'
import { availableParallelism } from 'node:os'
import { isMainThread, parentPort, workerData, Worker } from 'node:worker_threads'
import path from 'node:path'

/** web/ 根目录(本脚本位于 web/scripts/)。 */
const WEB_ROOT = path.resolve(import.meta.dirname, '..')
/** vite build 的输出目录。 */
const DEFAULT_DIST = path.join(WEB_ROOT, 'dist')

/** 值得预压缩的文本类扩展名。 */
const TEXT_EXTENSIONS = new Set([
  '.js',
  '.mjs',
  '.css',
  '.html',
  '.json',
  '.svg',
  '.map',
  '.txt',
  '.webmanifest',
])

/** 小于这个字节数不产出变体:收益抵不上文件数量。 */
const MIN_BYTES = 256

/** 压一个文件,返回它贡献的统计;压完反而更大就不产出变体。 */
function compressFile(file) {
  const data = readFileSync(file)
  const brotli = brotliCompressSync(data, {
    params: {
      [zlibConstants.BROTLI_PARAM_QUALITY]: 11,
      [zlibConstants.BROTLI_PARAM_SIZE_HINT]: data.length,
    },
  })
  const gzip = gzipSync(data, { level: 9 })
  const kept = []
  if (brotli.length < data.length) kept.push(['.br', brotli])
  if (gzip.length < data.length) kept.push(['.gz', gzip])
  if (kept.length === 0) return null
  for (const [suffix, bytes] of kept) writeFileSync(file + suffix, bytes)
  return {
    br: brotli.length < data.length,
    gz: gzip.length < data.length,
    raw: data.length,
    brBytes: Math.min(brotli.length, data.length),
    gzBytes: Math.min(gzip.length, data.length),
  }
}

/** worker 入口:压缩分到的文件,把统计回传主线程。 */
if (!isMainThread) {
  const stats = []
  for (const file of workerData.files) {
    const stat = compressFile(file)
    if (stat) stats.push(stat)
  }
  parentPort.postMessage(stats)
} else {
  await main()
}

async function main() {
  const dist = path.resolve(process.argv[2] ?? DEFAULT_DIST)
  if (!statOrNull(dist)?.isDirectory()) {
    console.error(`[precompress] 目录不存在:${dist}(先跑 vite build)`)
    process.exit(1)
  }

  const all = walk(dist)
  // 先清掉上一轮的旁挂产物。vite 的 emptyOutDir 覆盖整目录重建的情况,但增量
  // 构建或手工重跑本脚本时,源文件已改名/删除而旧变体残留下来会变成孤儿实体,
  // 所以每次都要以当前源文件集合为准重建。
  for (const file of all) if (/\.(br|gz)$/.test(file)) rmSync(file)

  // 按体积从大到小轮转分配:大文件先开工,worker 之间的收尾时间才接近。
  const targets = all
    .filter(file => !/\.(br|gz)$/.test(file))
    .filter(file => TEXT_EXTENSIONS.has(path.extname(file).toLowerCase()))
    .filter(file => statSync(file).size >= MIN_BYTES)
    .sort((a, b) => statSync(b).size - statSync(a).size)
  const workerCount = Math.max(1, Math.min(availableParallelism(), targets.length))
  const buckets = Array.from({ length: workerCount }, () => [])
  targets.forEach((file, index) => buckets[index % workerCount].push(file))

  const stats = (
    await Promise.all(
      buckets.map(files => runWorker(files)).filter(Boolean),
    )
  ).flat()

  let rawTotal = 0
  let brTotal = 0
  let gzTotal = 0
  for (const stat of stats) {
    rawTotal += stat.raw
    brTotal += stat.brBytes
    gzTotal += stat.gzBytes
  }

  // 落地校验:产物数量必须与预期一致。半截产物(压缩中途失败)比没有产物更难
  // 排查 —— 服务端会静默回退到原样传输,构建却显示成功。
  const brExpected = stats.filter(stat => stat.br).length
  const gzExpected = stats.filter(stat => stat.gz).length
  const actual = walk(dist)
  const brActual = actual.filter(file => file.endsWith('.br')).length
  const gzActual = actual.filter(file => file.endsWith('.gz')).length
  if (brActual !== brExpected || gzActual !== gzExpected) {
    console.error(
      `[precompress] 落地校验失败:期望 ${brExpected} 个 .br / ${gzExpected} 个 .gz,` +
        `实际 ${brActual} 个 .br / ${gzActual} 个 .gz`,
    )
    process.exit(1)
  }

  const kb = n => (n / 1024).toFixed(0)
  console.log(
    `[precompress] ${stats.length} 个文本产物: ${kb(rawTotal)} KB → brotli ${kb(brTotal)} KB (−${pct(
      rawTotal,
      brTotal,
    )}%),gzip ${kb(gzTotal)} KB (−${pct(rawTotal, gzTotal)}%)`,
  )
}

function runWorker(files) {
  if (files.length === 0) return null
  return new Promise((resolve, reject) => {
    const worker = new Worker(new URL(import.meta.url), { workerData: { files } })
    worker.once('message', resolve)
    worker.once('error', reject)
    worker.once('exit', code => {
      if (code !== 0) reject(new Error(`压缩线程退出码 ${code}`))
    })
  })
}

function statOrNull(p) {
  try {
    return statSync(p)
  } catch {
    return null
  }
}

function walk(dir) {
  const out = []
  for (const entry of readdirSync(dir)) {
    const full = path.join(dir, entry)
    const stat = statSync(full)
    if (stat.isDirectory()) out.push(...walk(full))
    else out.push(full)
  }
  return out
}

function pct(a, b) {
  return a === 0 ? '0' : (((a - b) / a) * 100).toFixed(0)
}
