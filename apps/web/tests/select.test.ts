import { describe, expect, test } from 'bun:test'
import { InkBuffers, type Ev, type Item, type Pt, type StrokeItem } from '../src/canvas/ink'
import { fold, piecesIn } from '../src/canvas/select'

const line = (x0: number, x1: number, y = 0, n = 11): Pt[] => Array.from({ length: n }, (_, i) => [x0 + ((x1 - x0) * i) / (n - 1), y, 0.5, 0] as Pt)
const stroke = (id: string, tool: 'pen' | 'pencil', x0: number, x1: number, y = 0): Ev => ({ type: 'stroke', id, tool, seed: 1, size: 4, pts: line(x0, x1, y) })
const eraser = (id: string, x0: number, x1: number, op = 1, only?: string[]): Ev => ({ type: 'stroke', id, tool: 'eraser', seed: 1, size: 5, op, pts: line(x0, x1, 0, 5), only })
const box = (x0: number, y0: number, x1: number, y1: number): [number, number][] => [[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
const strokes = (items: Item[]) => items.filter((i): i is StrokeItem => i.tool !== 'image')
const xs = (it: StrokeItem) => it.pts.map((p) => p[0])
const ALL = { pencil: true, pen: true, image: true }

describe('a selection only holds what was selected', () => {
  test('moving a selection over other ink does not pick that ink up, even on a second move', () => {
    const A = stroke('A', 'pen', 0, 10)
    const B = stroke('B', 'pen', 100, 110)
    // select A (ids), move it onto B
    const sel1 = piecesIn(fold([A, B]), box(-5, -5, 15, 5), ALL)
    expect(sel1.ids).toEqual(['A'])
    const xf = { dx: 100, dy: 0, s: 1, cx: 5, cy: 0 }
    const m1: Ev = { type: 'move', id: 'm1', poly: box(-5, -5, 15, 5), xf, ids: sel1.ids }
    const items1 = strokes(fold([A, B, m1]))
    expect(items1.find((i) => i.id === 'B')?.pts.length).toBe(11)
    // the new selection is the moved pieces, not whatever sits in the new box
    const moved = items1.filter((i) => i.id.startsWith('A~m1i'))
    expect(moved.length).toBe(1)
    const ids2 = moved.map((m) => m.id)
    // move again, back left by 50: B must stay put
    const m2: Ev = { type: 'move', id: 'm2', poly: box(95, -5, 115, 5), xf: { dx: -50, dy: 0, s: 1, cx: 105, cy: 0 }, ids: ids2 }
    const items2 = strokes(fold([A, B, m1, m2]))
    expect(items2.find((i) => i.id === 'B')?.pts.map((p) => p[0])[0]).toBe(100)
    expect(Math.min(...xs(items2.find((i) => i.id.startsWith('A~m1i0~m2i'))!))).toBeCloseTo(50, 5)
  })

  test('a move with no ids behaves as before (older ops): everything inside the polygon', () => {
    const items = strokes(fold([stroke('A', 'pen', 0, 10), stroke('B', 'pen', 2, 8, 1), { type: 'move', id: 'm', poly: box(-5, -5, 15, 5), xf: { dx: 50, dy: 0, s: 1, cx: 0, cy: 0 } }]))
    expect(items.every((i) => Math.min(...xs(i)) >= 50)).toBe(true)
  })

  test('a delete with ids leaves other ink inside the polygon alone', () => {
    const items = strokes(fold([stroke('A', 'pen', 0, 10), stroke('B', 'pen', 2, 8, 1), { type: 'delete', id: 'd', poly: box(-5, -5, 15, 5), ids: ['A'] }]))
    expect(items.map((i) => i.id)).toEqual(['B'])
  })
})

describe('erased ink is not selected and does not come back when moved', () => {
  const A = stroke('A', 'pencil', 0, 100)
  const E = eraser('E', 40, 60) // wipes the middle (radius = 1.2 * size = 6)

  test('selecting across an erased gap holds only what is visible', () => {
    const sel = piecesIn(fold([A, E]), box(-5, -5, 105, 5), ALL)
    expect(sel.ids).toEqual(['A'])
    const pts = sel.pieces.flatMap((p) => (p.tool === 'image' ? [] : xs(p)))
    expect(pts.some((x) => x > 36 && x < 64)).toBe(false)
    expect(pts.some((x) => x <= 36)).toBe(true)
    expect(pts.some((x) => x >= 64)).toBe(true)
  })

  test('a fully erased stroke cannot be selected at all', () => {
    const sel = piecesIn(fold([stroke('A', 'pencil', 40, 60), E]), box(-5, -5, 105, 5), ALL)
    expect(sel.ids).toEqual([])
    expect(sel.pieces).toEqual([])
  })

  test('moving the selection carries no erased points along', () => {
    const sel = piecesIn(fold([A, E]), box(-5, -5, 105, 5), ALL)
    const items = strokes(fold([A, E, { type: 'move', id: 'm', poly: box(-5, -5, 105, 5), xf: { dx: 0, dy: 200, s: 1, cx: 50, cy: 0 }, ids: sel.ids }]))
    const moved = items.filter((i) => i.id.startsWith('A~mi'))
    expect(moved.length).toBeGreaterThan(0)
    for (const m of moved) for (const p of m.pts) expect(p[0] <= 36 || p[0] >= 64).toBe(true)
  })

  test('a faint (partial) erase leaves the stroke selectable', () => {
    const sel = piecesIn(fold([A, eraser('E2', 40, 60, 0.3)]), box(-5, -5, 105, 5), ALL)
    expect(sel.pieces.flatMap((p) => (p.tool === 'image' ? [] : xs(p))).some((x) => x > 45 && x < 55)).toBe(true)
  })

  test('an eraser only reaches the layers that were visible when it was used', () => {
    const penLine = stroke('P', 'pen', 40, 60)
    const pencilOnly = eraser('E3', 40, 60, 1, ['pencil'])
    expect(piecesIn(fold([penLine, pencilOnly]), box(-5, -5, 105, 5), ALL).ids).toEqual(['P'])
    expect(piecesIn(fold([stroke('Q', 'pencil', 40, 60), pencilOnly]), box(-5, -5, 105, 5), ALL).ids).toEqual([])
  })
})

describe('a straight stroke is two points, but its erased middle is still found', () => {
  test('selecting and moving it leaves the gap', () => {
    const A: Ev = { type: 'stroke', id: 'A', tool: 'pen', seed: 1, size: 4, pts: [[0, 0, 0.5, 0], [100, 0, 0.5, 0]] }
    const E = eraser('E', 40, 60)
    const sel = piecesIn(fold([A, E]), box(-5, -5, 105, 5), ALL)
    const xsSel = sel.pieces.flatMap((p) => (p.tool === 'image' ? [] : xs(p)))
    expect(xsSel.some((x) => x > 38 && x < 62)).toBe(false)
    expect(Math.min(...xsSel)).toBeLessThan(5)
    expect(Math.max(...xsSel)).toBeGreaterThan(95)
    const items = strokes(fold([A, E, { type: 'move', id: 'm', poly: box(-5, -5, 105, 5), xf: { dx: 0, dy: 50, s: 1, cx: 50, cy: 0 }, ids: sel.ids }]))
    for (const m of items.filter((i) => i.id.startsWith('A~mi'))) for (const p of m.pts) expect(p[0] < 40 || p[0] > 60).toBe(true)
  })
})

describe('the eraser only touches the layers it was drawn with', () => {
  test('erase() with only=[pencil] leaves the pen layer alone', () => {
    const b = new InkBuffers()
    b.resize(60, 60, 1)
    b.clear({ x: 0, y: 0, z: 1 })
    b.stampStroke({ tool: 'pencil', seed: 1, size: 8, pts: line(10, 50, 30) })
    b.stampStroke({ tool: 'pen', seed: 2, size: 8, pts: line(10, 50, 30) })
    const pen0 = b.covPen[30 * 60 + 30]
    const pencil0 = b.covPencil[30 * 60 + 30]
    expect(pen0).toBeGreaterThan(0.5)
    expect(pencil0).toBeGreaterThan(0.05)
    b.erase(30, 30, 12, 1, ['pencil'])
    expect(b.covPen[30 * 60 + 30]).toBe(pen0)
    expect(b.covPencil[30 * 60 + 30]).toBe(0)
    b.erase(30, 30, 12, 1, ['pen'])
    expect(b.covPen[30 * 60 + 30]).toBe(0)
  })
})
