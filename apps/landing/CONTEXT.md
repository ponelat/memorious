# memorious.app — landing page context

The public face of Memorious. One self-contained `index.html` + `hero.mp4` + `poster.jpg`,
served as static files from the shared EC2 Caddy box (deploy recipe: docs/DEPLOY.md).
No framework, no build step, no analytics.

## The name

**Memorious**: a rare English adjective, "having a great memory". Chosen 2026-08-11 after a
naming round (Edda was the runner-up — warm, but edda.app was taken and the word resisted
being an entity). Radio-test script if anyone asks: *"Memorious — like glorious, but with
memory."* The Borges connection ("Funes the Memorious") is deliberately NOT part of the
brand — the owner prefers the word without the story. Don't add it to the page.

## Voice & design

- Same design language as the app: paper `#fbfaf8`, ink `#161616`, faint `#8a8781`,
  line `#e4e1db`, ui-monospace stack, lowercase wordmark with wide tracking.
- Copy voice: quiet, declarative, no marketing superlatives, no exclamation marks.
  Tagline: "A quiet place for what happened."
- Page structure: hero (wordmark, tagline, framed video) → "what it believes" manifesto
  (append-only / local-first / no accounts / peer-to-peer / nothing asked of you) →
  "how it works" three columns (capture / sync / find) → one-line footer.
- No public CTA by design — the product is personal; there's nothing to sign up for.

## Hero video

15s silent loop, 1080p, ~480KB, autoplay/muted/loop with `poster.jpg`. Shots: login →
passcode → stream → typing a note → note added → photo-fan lightbox → sync view.

**Content policy: never film the owner's real journal.** The video is captured from a
throwaway staged demo journal with invented entries and generated abstract "photos".

Regeneration recipe (used 2026-08-11):

1. Stage: init a scratch journal + passcode `1234`; run
   `MEMORIOUS_DATA=<scratch> PORT=4602 WEB_DIST=apps/web/dist target/release/memorious-server`;
   seed via the HTTP API — a few short text entries, three generated gradient PNGs
   (photo-fan needs ≥3 consecutive photos), one `say`-synthesized m4a (the sweeper
   transcribes it, which shows the annotation UI).
2. Capture: headless Chrome via `bdg` (browser-debugger-cli). Set viewport
   `Emulation.setDeviceMetricsOverride {width:1440,height:810,deviceScaleFactor:2}`,
   then walk the shot list with `dom fill/click/screenshot` into `frames/NN-name.png`.
   Watch for duplicate seeded entries between takes — redact strays first.
3. Stitch: `python3 scripts/stitch-hero.py <frames-dir> apps/landing/`.
4. Deploy per docs/DEPLOY.md.

## Voiceover (pending)

The owner intends to record a voiceover (script/pacing TBD). When the audio lands:
mux it over a (possibly re-cut) visual track, then **switch the player to click-to-play
with sound** — browsers block audible autoplay, so the current
`autoplay muted loop` attributes must be replaced with a play control; keep the silent
loop as the pre-click state if it still reads well.

## SEO / meta

Title "memorious — a quiet place for what happened"; og:title/description/image are set
(og:image = https://memorious.app/poster.jpg). The word "memorious" is rare enough that
ranking needs nothing more than existing.
