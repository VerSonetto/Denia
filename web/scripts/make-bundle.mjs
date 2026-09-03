// 换个更直接的复现:esbuild 打包 App + 极简 jsdom 环境,直接 render。
import { build } from 'esbuild'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const result = await build({
  stdin: {
    contents: "import App from '../src/App.tsx'\nexport default App\n",
    resolveDir: root,
    loader: 'tsx',
  },
  bundle: true,
  write: false,
  format: 'esm',
  platform: 'browser',
  jsx: 'automatic',
  define: { 'process.env.NODE_ENV': JSON.stringify('development'), 'import.meta.env': '{}' },
  external: ['vite'],
  loader: { '.css': 'empty', '.woff': 'dataurl', '.woff2': 'dataurl', '.ttf': 'dataurl' },
  alias: { react: path.join(root, 'node_modules/react/index.js').replace(/\\\\/g, '/'), 'react-dom/client': path.join(root, 'node_modules/react-dom/client.js').replace(/\\\\/g, '/'), 'react-dom': path.join(root, 'node_modules/react-dom/index.js').replace(/\\\\/g, '/') },
  sourcemap: 'inline',
  outExtension: { '.js': '.mjs' },
})
await import('node:fs').then(fs => fs.writeFileSync(path.join(root, 'app-bundle.mjs'), result.outputFiles[0].text))
console.log('bundle written, size:', result.outputFiles[0].text.length)