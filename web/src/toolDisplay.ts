/** Human-readable tool call labels; short paths instead of full absolute paths. */

function normalizeSlashes(path: string): string {
  return path.replace(/\\/g, '/').replace(/\/+/g, '/')
}

/** Collapse long absolute paths to a short, relative-looking tail. */
export function shortPath(raw: string): string {
  const trimmed = raw.trim()
  if (!trimmed) return trimmed
  let path = normalizeSlashes(trimmed)
  path = path.replace(/^[A-Za-z]:\//, '')
  if (path.startsWith('/')) path = path.slice(1)
  const parts = path.split('/').filter(Boolean)
  if (parts.length <= 2) return parts.join('/')
  return `…/${parts.slice(-2).join('/')}`
}

function parseArgsObject(args: string): Record<string, unknown> | null {
  const start = args.indexOf('{')
  if (start < 0) return null
  let depth = 0
  for (let i = start; i < args.length; i++) {
    const ch = args[i]
    if (ch === '{') depth++
    else if (ch === '}') {
      depth--
      if (depth === 0) {
        try {
          return JSON.parse(args.slice(start, i + 1)) as Record<string, unknown>
        } catch {
          return null
        }
      }
    }
  }
  return null
}

function readString(obj: Record<string, unknown>, key: string): string | undefined {
  const value = obj[key]
  return typeof value === 'string' ? value : undefined
}

/** One-line summary for a tool row header. */
export function toolCallSummary(name: string, args: string): string {
  const parsed = parseArgsObject(args)
  if (parsed) {
    switch (name) {
      case 'read_file':
      case 'write_file': {
        const path = readString(parsed, 'path')
        if (path) return shortPath(path)
        break
      }
      case 'bash': {
        const command = readString(parsed, 'command')
        if (command) return command.length > 90 ? `${command.slice(0, 90)}…` : command
        break
      }
      case 'glob':
      case 'grep': {
        const pattern = readString(parsed, 'pattern')
        if (pattern) return pattern.length > 90 ? `${pattern.slice(0, 90)}…` : pattern
        break
      }
      case 'edit': {
        const path = readString(parsed, 'path')
        if (path) return shortPath(path)
        break
      }
    }
  }
  const line = args.trim().split('\n')[0]?.trim() ?? ''
  return line.length > 90 ? `${line.slice(0, 90)}…` : line
}

/** Compact input body when a tool row is expanded. */
export function toolCallInput(name: string, args: string): string | null {
  const parsed = parseArgsObject(args)
  if (!parsed) return args.trim() || null
  switch (name) {
    case 'read_file': {
      const path = readString(parsed, 'path')
      return path ? shortPath(path) : null
    }
    case 'write_file': {
      const path = readString(parsed, 'path')
      const content = readString(parsed, 'content')
      if (!path) return null
      const head = shortPath(path)
      if (!content) return head
      const preview =
        content.length > 240 ? `${content.slice(0, 240).trimEnd()}…` : content
      return `${head}\n\n${preview}`
    }
    case 'bash': {
      const command = readString(parsed, 'command')
      return command ?? null
    }
    default:
      return null
  }
}
