// 构建期预压缩控制台产物:给每个文本文件旁挂 .br 与 .gz 实体变体。
//
// 为什么放在构建期而不是运行时压缩:经公网隧道发到手机上时**字节数就是延迟**
// (主包 1.5 MB 级,弱网上多跑几百 KB 就是几百毫秒)。构建期可以随便用
// brotli q11 换最小体积,运行时零 CPU、零缓冲;服务端只按 Accept-Encoding
// 选一个现成实体发出去(见 crates/server/src/web_assets.rs)。
//
// 跳过图片与字体:它们自身已压缩,再压一遍纯属浪费(woff2 内部已是 Brotli)。

import { brotliCompressSync, gzipSync, constants as zlibConstants } from 'node:zlib'
import { readdirSync, statSync, readFileSync, writeFileSync, rmSync } from 'node:fs'
import path from 'node:path'

const REPO_ROOT = path.resolve(import.meta.dirname, '..')
const DEFAULT_DIST = path.join(REPO_ROOT, 'web', 'dist')

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

function main() {
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

  let count = 0
  let rawTotal = 0
  let brTotal = 0
  let gzTotal = 0
  for (const file of all) {
    if (/\.(br|gz)$/.test(file)) continue
    if (!TEXT_EXTENSIONS.has(path.extname(file).toLowerCase())) continue
    const data = readFileSync(file)
    if (data.length < MIN_BYTES) continue

    const brotli = brotliCompressSync(data, {
      params: {
        [zlibConstants.BROTLI_PARAM_QUALITY]: 11,
        [zlibConstants.BROTLI_PARAM_SIZE_HINT]: data.length,
      },
    })
    const gzip = gzipSync(data, { level: 9 })
    // 压完反而更大(已压缩内容、极小文件)就不产出变体,让服务端原样发。
    const kept = []
    if (brotli.length < data.length) kept.push(['.br', brotli])
    if (gzip.length < data.length) kept.push(['.gz', gzip])
    if (kept.length === 0) continue
    for (const [suffix, bytes] of kept) writeFileSync(file + suffix, bytes)

    count += 1
    rawTotal += data.length
    brTotal += Math.min(brotli.length, data.length)
    gzTotal += Math.min(gzip.length, data.length)
  }

  const kb = (n) => (n / 1024).toFixed(0)
  console.log(
    `[precompress] ${count} 个文本产物: ${kb(rawTotal)} KB → brotli ${kb(brTotal)} KB (−${pct(
      rawTotal,
      brTotal,
    )}%),gzip ${kb(gzTotal)} KB (−${pct(rawTotal, gzTotal)}%)`,
  )
}

function statOrNull(p) {
  try {
    return statSync(p)
  } catch {
    return null
  }
}

function pct(a, b) {
  return a === 0 ? '0' : (((a - b) / a) * 100).toFixed(0)
}

main()
