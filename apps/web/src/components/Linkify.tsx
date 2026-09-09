import { MouseEvent, ReactNode } from 'react'
import { inTauri } from '../api'

/** http(s) URLs and bare www. hosts, as people paste them into a note. */
const URL_RE = /\b(?:https?:\/\/|www\.)[^\s<>"'`]+/gi
const TRAILING = /[.,;:!?'"’”)\]}]+$/

/**
 * Splits a run of text into text and <a> nodes. Trailing punctuation stays
 * text (a URL at the end of a sentence), and a close-paren is only part of the
 * link when the link itself opened one (Wikipedia-style URLs).
 */
export function linkify(text: string): ReactNode[] {
  const out: ReactNode[] = []
  let last = 0
  for (const m of text.matchAll(URL_RE)) {
    let url = m[0]
    const start = m.index ?? 0
    const trail = url.match(TRAILING)?.[0] ?? ''
    if (trail) {
      let keep = url.length - trail.length
      // give back one ')' per unmatched '(' inside the url
      const opens = (url.slice(0, keep).match(/\(/g) ?? []).length
      const closes = (url.slice(0, keep).match(/\)/g) ?? []).length
      for (let i = 0; i < trail.length && opens > closes + i && trail[i] === ')'; i++) keep++
      url = url.slice(0, keep)
    }
    if (start > last) out.push(text.slice(last, start))
    const href = url.startsWith('www.') ? `https://${url}` : url
    out.push(
      <a key={start} className="link" href={href} target="_blank" rel="noopener noreferrer" onClick={open}>
        {url}
      </a>,
    )
    last = start + url.length
  }
  if (last < text.length) out.push(text.slice(last))
  return out
}

/** In the desktop app the webview drops new-window navigations; hand the URL to the OS. */
function open(e: MouseEvent<HTMLAnchorElement>) {
  e.stopPropagation()
  if (!inTauri) return
  e.preventDefault()
  const href = e.currentTarget.href
  void import('@tauri-apps/plugin-opener').then((m) => m.openUrl(href))
}
