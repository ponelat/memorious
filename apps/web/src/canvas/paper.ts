/** Paper tooth: a 256² world-anchored tile of fine noise + a few cellulose fibres, rank-normalised
 *  to uniform [0,1]. Built once at load. Ported from the ink-spike prototype. */
export function mulberry32(a: number) {
  return () => {
    a |= 0
    a = (a + 0x6d2b79f5) | 0
    let t = Math.imul(a ^ (a >>> 15), 1 | a)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

export const PAPER_N = 256

function build(): Float32Array {
  const n = PAPER_N
  const rnd = mulberry32(4242)
  const f = new Float32Array(n * n)
  for (let k = 0; k < 900; k++) {
    const x = rnd() * n
    const y = rnd() * n
    const ang = rnd() * 6.2832
    const len = 5 + rnd() * 18
    const w = 0.5 + rnd() * 0.8
    const c = Math.cos(ang)
    const s = Math.sin(ang)
    for (let t = 0; t < len; t += 0.7) {
      const px = x + c * t
      const py = y + s * t
      for (let dy = -1; dy <= 1; dy++)
        for (let dx = -1; dx <= 1; dx++) {
          const ix = Math.floor(px) + dx
          const iy = Math.floor(py) + dy
          const d = Math.hypot(px - ix - 0.5, py - iy - 0.5)
          f[(((iy % n) + n) % n) * n + (((ix % n) + n) % n)] += Math.max(0, w - d) * (1 - (t / len) * 0.5)
        }
    }
  }
  const rank = (src: Float32Array) => {
    const idx = Array.from(src.keys()).sort((p, q) => src[p] - src[q])
    const o = new Float32Array(n * n)
    idx.forEach((k, r) => (o[k] = r / (n * n - 1)))
    return o
  }
  const blur = (src: Float32Array) => {
    const o = new Float32Array(n * n)
    for (let y = 0; y < n; y++)
      for (let x = 0; x < n; x++) {
        let t = 0
        for (let dy = -1; dy <= 1; dy++)
          for (let dx = -1; dx <= 1; dx++)
            t += src[((y + dy) & 255) * n + ((x + dx) & 255)] * (dx || dy ? 1 : 2)
        o[y * n + x] = t / 10
      }
    return o
  }
  const wn = new Float32Array(n * n)
  for (let i = 0; i < wn.length; i++) wn[i] = rnd()
  const fine = rank(blur(wn))
  let lump = wn
  for (let i = 0; i < 3; i++) lump = blur(lump)
  lump = rank(lump)
  const fr = rank(f)
  const mix = new Float32Array(n * n)
  for (let i = 0; i < mix.length; i++) mix[i] = 0.6 * (0.85 * fine[i] + 0.15 * lump[i]) + 0.4 * fr[i]
  return rank(mix)
}

let paper: Float32Array | null = null
/** Lazy: the first canvas to mount pays the ~100 ms, nobody else does. */
export function getPaper(): Float32Array {
  return (paper ??= build())
}
