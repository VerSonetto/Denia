import React from 'react'
import ReactDOM from 'react-dom/client'
import App from './App'
import { ErrorBoundary } from './components/ErrorBoundary'
import './styles.css'
import './styles.traj.css'
import './markdown.css'

// 全局禁用浏览器默认右键菜单。需要自定义菜单的组件在 onContextMenu 里
// preventDefault 后自行渲染;这里兜住其余所有区域(包括输入框与正文)。
window.addEventListener('contextmenu', (event) => {
  event.preventDefault()
})

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    {/* 顶层兜底:任何渲染期异常都不该让整页变白(React 18 会卸载整棵根树)。 */}
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
)
