import type { ReactNode } from 'react'

type IconProps = { size?: number; className?: string }

function Icon({ size = 16, className, children }: IconProps & { children: ReactNode }) {
  return (
    <svg width={size} height={size} viewBox="0 0 20 20" fill="none" aria-hidden="true" className={className} style={{ flex: 'none', display: 'block' }}>
      <g stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round">{children}</g>
    </svg>
  )
}

export function IconSettingsGeneral(props: IconProps) {
  return <Icon {...props}><path d="M3 5h5m4 0h5M3 10h9m4 0h1M3 15h2m4 0h8" /><circle cx="10" cy="5" r="2" /><circle cx="14" cy="10" r="2" /><circle cx="7" cy="15" r="2" /></Icon>
}

export function IconSettingsAgents(props: IconProps) {
  return <Icon {...props}><rect x="7" y="2.5" width="6" height="5" rx="1.2" /><rect x="2.5" y="12" width="6" height="5" rx="1.2" /><rect x="11.5" y="12" width="6" height="5" rx="1.2" /><path d="M10 7.5V10m-4.5 2v-2h9v2" /></Icon>
}

export function IconSettingsPresets(props: IconProps) {
  return <Icon {...props}><rect x="3" y="3" width="6" height="6" rx="1.2" /><rect x="12" y="3" width="5" height="6" rx="1.2" /><rect x="3" y="12" width="6" height="5" rx="1.2" /><path d="M14.5 12v5M12 14.5h5" /></Icon>
}

export function IconSettingsPrompt(props: IconProps) {
  return <Icon {...props}><path d="M7 16H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-4l-4 2v-2Z M6 7h8M6 11h5" /></Icon>
}

export function IconSettingsModels(props: IconProps) {
  return <Icon {...props}><rect x="5" y="5" width="10" height="10" rx="2" /><rect x="8" y="8" width="4" height="4" rx=".5" /><path d="M8 2.5V5m4-2.5V5M8 15v2.5m4-2.5v2.5M2.5 8H5m-2.5 4H5m10-4h2.5M15 12h2.5" /></Icon>
}

export function IconSettingsMemory(props: IconProps) {
  return <Icon {...props}><path d="M5 3h10a1 1 0 0 1 1 1v13H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2Zm0 0v14M8 7h5M8 10h4" /></Icon>
}

export function IconSettingsRemote(props: IconProps) {
  return <Icon {...props}><rect x="2.5" y="3" width="15" height="10.5" rx="1.5" /><path d="M7 17h6m-3-3.5V17M7 8.5h6m-2-2 2 2-2 2" /></Icon>
}

export function IconSettingsChannels(props: IconProps) {
  return <Icon {...props}><path d="M2.5 8.2c0-2.6 2.9-4.7 6.5-4.7s6.5 2.1 6.5 4.7-2.9 4.7-6.5 4.7c-.6 0-1.2 0-1.8-.1L4 14.6l.8-2.3C3.5 11.4 2.5 9.9 2.5 8.2Z" /><path d="M11.5 12.6c.6 1.7 2.5 2.9 4.7 2.9.5 0 1-.1 1.5-.2l2 1.1-.6-1.8c.7-.5 1.1-1.3 1.1-2.1 0-1.8-1.8-3.3-4-3.3-.4 0-.9 0-1.3.1" /></Icon>
}

export function IconSettingsRuntime(props: IconProps) {
  return <Icon {...props}><circle cx="10" cy="10" r="7" /><path d="M10 5.5V10l3 2" /></Icon>
}

export function IconSettingsAppearance(props: IconProps) {
  return <Icon {...props}><circle cx="10" cy="10" r="7" /><path d="M10 3v14m0-10h5.7M10 10h7m-7 3h5.7" /></Icon>
}

export function IconPermissionRead(props: IconProps) {
  return <Icon {...props}><path d="M2 10s2.8-5.5 8-5.5 8 5.5 8 5.5-2.8 5.5-8 5.5S2 10 2 10Z" /><circle cx="10" cy="10" r="2.5" /></Icon>
}

export function IconPermissionEdit(props: IconProps) {
  return <Icon {...props}><path d="m4 12 9-9 4 4-9 9-5 1 1-5Zm6.5-6.5 4 4M4 12l4 4" /></Icon>
}

export function IconPermissionPlan(props: IconProps) {
  return <Icon {...props}><rect x="4" y="3.5" width="12" height="14" rx="1.5" /><path d="M7.5 2.5h5v3h-5zM7 9h1m3 0h2m-6 4h1m3 0h2" /></Icon>
}

export function IconPermissionFull(props: IconProps) {
  return <Icon {...props}><path d="M10 2.5 17 5v5c0 4-4 6.5-7 8-3-1.5-7-4-7-8V5l7-2.5Z M7 10l2 2 4-4" /></Icon>
}
