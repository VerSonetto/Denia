/**
 * 畸形 ATX 标题修复的回归网。
 *   node scripts/check-markdown-repair.mjs
 *
 * 模型常把标题写成 `##验证结果`(漏空格)或 `##验证结果| 检查项 | 结果 |`
 * (标题与表格表头黏在一行),CommonMark 会把它们整段降级成段落,井号与表格
 * 分隔行就原样露在正文里。这个脚本盯住修复器的三类行为:
 *
 *   1) 该修的修对:补空格、拆黏连、恢复被吞的表格;
 *   2) 不该动的别动:shebang、预处理器、围栏内的 `#`、本来就合法的标题;
 *   3) 流式与定稿同解:增量修复逐段喂入的结果,必须与全量修复逐字一致
 *      ——否则同一条消息在流式期间和收尾后会渲染成两个样子。
 */
import { mkdirSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'

const here = dirname(fileURLToPath(import.meta.url))
const webRoot = join(here, '..')
const outDir = join(webRoot, 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const bundle = join(outDir, 'repair-headings.mjs')

await build({
  entryPoints: [join(webRoot, 'src', 'markdown', 'repairHeadings.ts')],
  outfile: bundle,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  logLevel: 'silent',
})

const { repairMalformedHeadings, HeadingRepair } = await import(
  `${pathToFileURL(resolve(bundle)).href}?t=${Date.now()}`
)

let failed = 0
function check(label, actual, expected) {
  const a = JSON.stringify(actual)
  const e = JSON.stringify(expected)
  if (a !== e) {
    failed++
    console.log(`FAIL ${label}\n  actual   ${a}\n  expected ${e}`)
  } else {
    console.log(`ok   ${label}`)
  }
}

/** 断言修复结果等于给定文本。 */
function fixed(label, input, expected) {
  check(label, repairMalformedHeadings(input), expected)
}

/* 1) 该修的修对。 */
fixed('漏空格 → 补空格', '##验证结果\n\n正文', '## 验证结果\n\n正文')
fixed('标题与表格表头黏连 → 断行后表格恢复', '##验证结果| 检查项 | 结果 |\n|---|---|\n| /health | ok |',
  '## 验证结果\n| 检查项 | 结果 |\n|---|---|\n| /health | ok |')
fixed('标题与加粗正文黏连 → 在加粗处断行',
  '##改了什么**每次对话结束后自动刷新该账号余额**，实现上注意了两个细节：',
  '## 改了什么\n**每次对话结束后自动刷新该账号余额**，实现上注意了两个细节：')
fixed('两个标题黏连 → 各自成行',
  '##1.分段架构###1.1两个维度每段是一个 `Section`对象',
  '## 1.分段架构\n### 1.1两个维度每段是一个 `Section`对象')
fixed('超长正文黏连 → 去掉标题标记按正文渲染',
  '##根因日志里模型输出的是 `<tool_call><function=ls>...`这种**文本**——这不是模型不会用工具。',
  '根因日志里模型输出的是 `<tool_call><function=ls>...`这种**文本**——这不是模型不会用工具。')
fixed('短标题带问号仍是标题', '##为什么？\n\n正文', '## 为什么？\n\n正文')

/* 2) 不该动的别动。 */
fixed('合法标题原样保留', '## 正常的标题\n\n正文', '## 正常的标题\n\n正文')
fixed('shebang 不动', '#!/usr/bin/env bash\nls', '#!/usr/bin/env bash\nls')
fixed('预处理器指令不动', '##include <stdio.h>', '##include <stdio.h>')
fixed('围栏内的 # 不动', '```\n##not a heading\n```', '```\n##not a heading\n```')
fixed('无畸形时原样返回(含 HTML 注释)', '<!-- ##x -->\n正文', '<!-- ##x -->\n正文')

/* 3) 整行就是标题的形态:补空格但不在行内代码处劈开。 */
fixed('标题内含行内代码(整行是标题)', '##交付物(都在 `D:\\zcode-mod\\`)', '## 交付物(都在 `D:\\zcode-mod\\`)')
fixed('编号开头 + 行内代码(整行是标题)',
  '###3.2 `# Harness`（identity段，server.js:73840，stable）',
  '### 3.2 `# Harness`（identity段，server.js:73840，stable）')

/* 4) 流式增量与定稿全量必须同解。 */
const streamingCases = [
  '##验证结果| 检查项 | 结果 |\n|---|---|\n| /health | ok |\n\n后续段落。',
  '##最大的三处收益**1.流式期间每帧重渲染被掐掉了。**原来每来一个流式帧都会重算。\n\n##有意没做的（需要你先定口径）',
  '前言。\n\n##1.分段架构###1.1两个维度每段是一个 `Section`对象\n\n###3.2 `# Harness`（identity段，stable）',
  '```bash\n##include 在围栏里\n```\n\n##验证`cargo test`全绿；`pnpm check`通过。',
]
for (const [index, text] of streamingCases.entries()) {
  const full = repairMalformedHeadings(text)
  // 按小块喂入,模拟流式:每个前缀都要能收敛到与定稿一致的结果。
  const repair = new HeadingRepair()
  for (let cut = 1; cut <= text.length; cut += 3) repair.repair(text.slice(0, cut))
  check(`流式增量与定稿同解 #${index + 1}`, repair.repair(text), full)
}

// 非追加输入(重试生成 / 切换消息)必须丢弃缓存重来,不能残留上一条的定型结果。
const reuse = new HeadingRepair()
reuse.repair('##第一条| a | b |\n|---|---|\n| 1 | 2 |')
check('非追加输入丢弃缓存', reuse.repair('##第二条\n\n正文'), '## 第二条\n\n正文')

console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 全部通过')
