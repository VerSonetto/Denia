// Node loader:strip TS 类型 + 转换 JSX,让 web/src 图能直接被 Node 加载。
import { readFile } from 'node:fs/promises'
import { fileURLToPath, pathToFileURL } from 'node:url'

const jsxOption = { jsx: 'automatic', types: true }

function isTs(url) {
  if (!url.startsWith('file:')) return false
  const p = fileURLToPath(url)
  return p.endsWith('.ts') || p.endsWith('.tsx')
}

export async function load(url, context, nextLoad) {
  console.log('[load]', url, 'isTs=', isTs(url))
  if (!isTs(url)) return nextLoad(url, context)
  const source = await readFile(fileURLToPath(url), 'utf8')
  const ts = await import('typescript')
  const out = ts.transpileModule(source, {
    compilerOptions: {
      ...jsxOption,
      target: ts.ScriptTarget.ES2022,
      module: ts.ModuleKind.ESNext,
      moduleResolution: ts.ModuleResolutionKind.Bundler,
      verbatimModuleSyntax: true,
    },
    fileName: fileURLToPath(url),
  })
  return {
    format: 'module',
    source: out.outputText,
    shortCircuit: true,
  }
}

export async function resolve(specifier, context, nextResolve) {
  // 让无后缀/目录导入按 ts 解析(bundler 风格)。
  try {
    return await nextResolve(specifier, context)
  } catch (err) {
    if (context.parentURL && /^\.\/|^\.\.\//.test(specifier)) {
      const base = new URL(specifier, context.parentURL)
      for (const ext of ['.tsx', '.ts']) {
        try {
          return await nextResolve(base.href + ext, context)
        } catch {}
      }
    }
    throw err
  }
}