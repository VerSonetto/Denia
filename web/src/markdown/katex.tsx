import { createElement } from 'react'
import type { CSSProperties, ReactNode } from 'react'
import katex from 'katex'

/**
 * TeX→React via KaTeX, mirroring the rehype-katex chain: strict render, then
 * `strict: 'ignore'` retry, then an error span. KaTeX output is a static
 * span/MathML/SVG vocabulary with no raw user HTML, so mapping its parsed DOM
 * onto React elements carries the same trust as shiki's tree.
 */

/** One inline `style` attribute string → React style object (kebab-case only). */
function styleObject(css: string): CSSProperties {
  const style: Record<string, string> = {}
  for (const declaration of css.split(';')) {
    const colon = declaration.indexOf(':')
    if (colon === -1) continue
    const name = declaration.slice(0, colon).trim()
    const key = name.replace(/-([a-z])/g, (_, letter: string) => letter.toUpperCase())
    style[key] = declaration.slice(colon + 1).trim()
  }
  return style
}

/** Map one parsed DOM node onto a React element (text nodes pass through). */
function domToReact(node: ChildNode, key: number): ReactNode {
  if (node.nodeType === Node.TEXT_NODE) return node.textContent
  if (node.nodeType !== Node.ELEMENT_NODE) return null
  const element = node as Element
  const props: Record<string, unknown> = { key }
  for (const attribute of element.attributes) {
    if (attribute.name === 'class') props['className'] = attribute.value
    else if (attribute.name === 'style') props['style'] = styleObject(attribute.value)
    else props[attribute.name] = attribute.value
  }
  const children = [...element.childNodes].map(domToReact)
  return children.length === 0
    ? createElement(element.localName, props)
    : createElement(element.localName, props, ...children)
}

/**
 * Render TeX source to React elements through KaTeX.
 * @param value - The TeX source (fenced `math` blocks append their trailing
 * newline to match the replaced pipeline's text extraction).
 * @param displayMode - Display (block) versus inline rendering.
 * @returns KaTeX's element tree, or the error span when the source does not parse.
 */
export function renderTexToReact(value: string, displayMode: boolean): ReactNode {
  let html: string
  try {
    html = katex.renderToString(value, { displayMode, throwOnError: true })
  } catch (error) {
    try {
      html = katex.renderToString(value, { displayMode, strict: 'ignore', throwOnError: false })
    } catch {
      return (
        <span className="katex-error" style={{ color: '#cc0000' }} title={String(error)}>
          {value}
        </span>
      )
    }
  }
  const parsed = new DOMParser().parseFromString(html, 'text/html')
  return [...parsed.body.childNodes].map(domToReact)
}