import { ReactNode } from 'react'
import { Wordmark } from './Wordmark'

/** The pre-journal screens (setup, unlock, passcode), drawn from the landing
 * page and the iPhone app's BrandHeroCard: a warm photo band, a card
 * overlapping it that carries the handwritten wordmark, a headline, a quiet
 * subline, then the screen's own hairline fields and italic outlined buttons.
 * Line breaks in `headline`/`subline` are honoured (pre-line). */
export function BrandHero({
  headline,
  subline,
  children,
}: {
  headline: string
  subline: string
  children: ReactNode
}) {
  return (
    <div className="hero">
      <div className="hero-photo" aria-hidden="true" />
      <section className="hero-card">
        <Wordmark className="hero-mark" />
        <h1 className="hero-headline">{headline}</h1>
        <p className="hero-subline">{subline}</p>
        <div className="hero-body">{children}</div>
      </section>
    </div>
  )
}
