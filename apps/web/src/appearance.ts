import { useEffect } from 'react'

/** system / light / dark, chosen on the sync page and remembered per browser
 * (localStorage). Applied as `data-theme` on <html>; "system" removes it and
 * lets prefers-color-scheme decide (styles.css). Same three choices as the
 * iPhone app's appearance picker. */
export type Appearance = 'system' | 'light' | 'dark'

const KEY = 'appearance'
const EVENT = 'journal:appearance'

export function readAppearance(): Appearance {
  try {
    const v = localStorage.getItem(KEY)
    if (v === 'light' || v === 'dark') return v
  } catch {
    /* storage unavailable: system it is */
  }
  return 'system'
}

export function setAppearance(a: Appearance) {
  try {
    if (a === 'system') localStorage.removeItem(KEY)
    else localStorage.setItem(KEY, a)
  } catch {
    /* not remembered, still applied */
  }
  apply(a)
  window.dispatchEvent(new Event(EVENT))
}

function apply(a: Appearance) {
  const root = document.documentElement
  if (a === 'system') delete root.dataset.theme
  else root.dataset.theme = a
}

/** Applies the remembered appearance once at mount. */
export function useAppearance() {
  useEffect(() => {
    apply(readAppearance())
  }, [])
}

/** Re-renders the caller when the appearance changes (the picker itself). */
export function onAppearanceChange(cb: () => void): () => void {
  window.addEventListener(EVENT, cb)
  return () => window.removeEventListener(EVENT, cb)
}
