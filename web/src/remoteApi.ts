/**
 * 远程连接的 API 封装。
 *
 * 两条 listener 共用同一份端点:
 * - 主 listener(本机控制台)拿到的是**含 PIN 与带票据链接**的完整状态;
 * - 远程 listener(手机)只拿得到"基址 + 二维码",票据与 PIN 一律不下发。
 *
 * 兑换与 PIN 校验这两个端点对两边都开放 —— 手机正是靠它们在远程侧
 * 把票据换成会话 cookie。
 */

import type { RemoteExchangeResult, RemoteStatus } from './types'

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    headers: { 'content-type': 'application/json' },
    ...init,
  })
  let body: unknown = null
  try {
    body = await response.json()
  } catch {
    /* 非 JSON 响应(网关错误页等)按空体处理 */
  }
  if (!response.ok) {
    const error = (body as { error?: { code?: string; message?: string } } | null)?.error
    throw new Error(error?.message ?? `请求失败(${response.status})`)
  }
  return body as T
}

export function getRemoteStatus(): Promise<RemoteStatus> {
  return request<RemoteStatus>('/api/remote/status')
}

/** 开启局域网连接。`address` 为空时由后端选推荐网卡。 */
export function startLan(address?: string): Promise<RemoteStatus> {
  return request<RemoteStatus>('/api/remote/lan/start', {
    method: 'POST',
    body: JSON.stringify({ address: address ?? null }),
  })
}

export function stopLan(): Promise<RemoteStatus> {
  return request<RemoteStatus>('/api/remote/lan/stop', { method: 'POST' })
}

/** 切换局域网访问地址(不改绑定,只改生成链接用的地址)。 */
export function selectLanAddress(address: string): Promise<RemoteStatus> {
  return request<RemoteStatus>('/api/remote/lan/address', {
    method: 'POST',
    body: JSON.stringify({ address }),
  })
}

export function startTunnel(): Promise<RemoteStatus> {
  return request<RemoteStatus>('/api/remote/tunnel/start', { method: 'POST' })
}

export function stopTunnel(): Promise<RemoteStatus> {
  return request<RemoteStatus>('/api/remote/tunnel/stop', { method: 'POST' })
}

/** 重新签发票据(旧链接不失效,各自到期为止)。 */
export function refreshTicket(): Promise<RemoteStatus> {
  return request<RemoteStatus>('/api/remote/ticket/refresh', { method: 'POST' })
}

export function revokeRemoteSession(id: string): Promise<{ ok: boolean; revoked: boolean }> {
  return request('/api/remote/sessions/revoke', {
    method: 'POST',
    body: JSON.stringify({ id }),
  })
}

/** 「立即关闭隧道并吊销全部会话」。 */
export function disconnectAll(): Promise<{ ok: boolean; revoked: number }> {
  return request('/api/remote/disconnect-all', { method: 'POST' })
}

/**
 * 用票据兑换会话。
 *
 * 票据走请求体而不是 query:query 会被写进各级访问日志,票据是一次性
 * 凭据,不该出现在日志里。
 */
export function exchangeTicket(ticket: string): Promise<RemoteExchangeResult> {
  return request<RemoteExchangeResult>('/api/remote/exchange', {
    method: 'POST',
    body: JSON.stringify({ ticket }),
  })
}

export function submitPin(challenge: string, pin: string): Promise<RemoteExchangeResult> {
  return request<RemoteExchangeResult>('/api/remote/pin', {
    method: 'POST',
    body: JSON.stringify({ challenge, pin }),
  })
}

/** 退出当前远程会话(吊销 cookie 对应的会话)。 */
export function remoteLogout(): Promise<{ ok: boolean }> {
  return request('/api/remote/logout', { method: 'POST' })
}
