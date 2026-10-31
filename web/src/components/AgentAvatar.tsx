import { useEffect, useState } from 'react'

const KEY = 'denia.agent-avatars.v1'
export const AVATARS = ['explorer', 'developer', 'verifier', 'orbit', 'spark', 'leaf', 'fox', 'cloud', 'moon', 'robot', 'owl', 'cat', 'sun'] as const
export type AvatarId = typeof AVATARS[number]

function read(): Record<string, AvatarId> {
  try { return JSON.parse(localStorage.getItem(KEY) || '{}') } catch { return {} }
}
function key(id: string, cwd?: string | null) {
  return id.startsWith('project:') ? `${cwd ?? ''}:${id}` : id
}
export function saveAgentAvatar(id: string, cwd: string | null | undefined, avatar: AvatarId) {
  try {
    localStorage.setItem(KEY, JSON.stringify({ ...read(), [key(id, cwd)]: avatar }))
    window.dispatchEvent(new Event('denia-agent-avatars'))
  } catch { /* 本地存储不可用时不影响定义保存 */ }
}
export function getAgentAvatar(id: string, cwd?: string | null): AvatarId {
  const chosen = read()[key(id, cwd)]
  if ((AVATARS as readonly string[]).includes(chosen)) return chosen
  // 内置角色按职责给出稳定默认值；用户手动选择始终优先。
  const builtins: Record<string, AvatarId> = {
    'builtin:explore': 'explorer',
    'builtin:develop': 'developer',
    'builtin:verify': 'verifier',
  }
  if (builtins[id]) return builtins[id]
  let hash = 0
  for (const char of id) hash = (hash * 31 + char.charCodeAt(0)) >>> 0
  return AVATARS[hash % AVATARS.length]
}
export function useAgentAvatar(id: string, cwd?: string | null): AvatarId {
  const [version, setVersion] = useState(0)
  useEffect(() => {
    const update = () => setVersion((v) => v + 1)
    window.addEventListener('storage', update)
    window.addEventListener('denia-agent-avatars', update)
    return () => { window.removeEventListener('storage', update); window.removeEventListener('denia-agent-avatars', update) }
  }, [])
  void version
  return getAgentAvatar(id, cwd)
}

export function AgentAvatar({ avatar, size = 32 }: { avatar: AvatarId; size?: number }) {
  const index = AVATARS.slice(3).findIndex((item) => item === avatar)
  // 内置角色专属图形与可选预设共用网格、线宽和主题色。
  const builtins: Partial<Record<AvatarId, JSX.Element>> = {
    explorer: <><circle cx="18" cy="18" r="8" /><path d="m24 24 5 5M18 14v8m-4-4h8" /><path d="M27 10h3m-1.5-1.5v3" /></>,
    developer: <><rect x="9" y="10" width="22" height="20" rx="4" /><path d="M9 15h22M13 12.5h.1M16 12.5h.1m1 6-3 3 3 3m6-6 3 3-3 3m-2-7-2 8" /></>,
    verifier: <><path d="M20 8 29 12v7c0 6-3.5 10-9 13-5.5-3-9-7-9-13v-7z" /><path d="m15.5 20 3 3 6-6" /></>,
  }
  const faces = [
    <><circle cx="20" cy="20" r="10" /><ellipse cx="20" cy="20" rx="14" ry="5" transform="rotate(-32 20 20)" /></>,
    <><circle cx="20" cy="20" r="11" /><path d="m14.5 20 3.7 3.7 7.5-8" /></>,
    <><path d="M10 27c0-10 6-16 19-16 0 13-6 19-16 18" /><path d="m11 30 13-13" /></>,
    <><path d="m11 14 9-5 9 5v12l-9 5-9-5z" /><path d="m11 14 9 5 9-5M20 19v12" /></>,
    <><path d="M11 27h18M13 22h14M15 17h10M17 12h6" /></>,
    <><path d="M26 10a11 11 0 1 0 4 18A11 11 0 0 1 26 10Z" /></>,
    <><rect x="9" y="11" width="22" height="18" rx="4" /><path d="m16 16-4 4 4 4m8-8 4 4-4 4m-3-10-2 12" /></>,
    <><circle cx="20" cy="20" r="11" /><path d="m24.5 15.5-3 6-6 3 3-6z" /><circle cx="20" cy="20" r="1" /></>,
    <><path d="M11 12h18v16H11zM11 17h18M16 12v16" /></>,
    <><circle cx="20" cy="20" r="7" /><path d="M20 8v3m0 18v3M8 20h3m18 0h3M11.5 11.5l2.2 2.2m12.6 12.6 2.2 2.2m0-17-2.2 2.2M13.7 26.3l-2.2 2.2" /></>,
  ]
  return <svg width={size} height={size} viewBox="0 0 40 40" fill="none" aria-hidden="true" style={{ flex: 'none', display: 'block' }}>
    <rect x="1" y="1" width="38" height="38" rx="12" fill="var(--hover-fill)" stroke="var(--border-l2)" />
    <g stroke="var(--label-secondary)" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">{builtins[avatar] ?? faces[Math.max(0, index)]}</g>
  </svg>
}
