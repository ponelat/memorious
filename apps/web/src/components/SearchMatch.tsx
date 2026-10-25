import { createContext, Fragment, ReactNode, useContext } from 'react'
import { Entry } from '../api'

/** Words to highlight in the entry being rendered (keyword matches only). */
export const MarkTerms = createContext<string[]>([])

export function queryTerms(query: string): string[] {
  return query.split(/\s+/).map((w) => w.replace(/"/g, '')).filter(Boolean)
}

const escape = (w: string) => w.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')

/** Wraps whole-word, case-insensitive occurrences of `terms` in <mark>. */
export function Marked({ text }: { text: string }): ReactNode {
  const terms = useContext(MarkTerms)
  if (terms.length === 0) return text
  const re = new RegExp(`(?<![\\p{L}\\p{N}])(${terms.map(escape).join('|')})(?![\\p{L}\\p{N}])`, 'giu')
  return text.split(re).map((part, i) => (i % 2 ? <mark key={i}>{part}</mark> : <Fragment key={i}>{part}</Fragment>))
}

/** "words" / "meaning 62%" chips under a search result. */
export function MatchBadge({ match }: { match?: Entry['match'] }) {
  if (!match) return null
  return (
    <div className="match-badge">
      {match.keyword && <span className="kw">words</span>}
      {match.similarity != null && <span className="sem">meaning {Math.round(match.similarity * 100)}%</span>}
    </div>
  )
}
