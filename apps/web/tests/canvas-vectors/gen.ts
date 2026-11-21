// Generates vectors.json from the TS reference (memorious/apps/web/src/canvas/{oplog,select}.ts).
// Run:  bun gen.ts      (from this directory; writes ./vectors.json)
// Native ports (Swift now, Android later) load vectors.json and must reproduce every `expected` (floats within 1e-9).
import { writeFileSync } from 'node:fs'
import { MonthLog, toData, toWire, type Op, type OpData, type WireRecord } from '../../src/canvas/oplog'
import { bboxOf, fold, pip, piecesIn, simplifyPoly, splitByPoly, xfItem, xfPoly, layerOf } from '../../src/canvas/select'
import type { Ev, Item, Layers, Poly, Pt, Xf } from '../../src/canvas/ink'

type Case = { name: string; kind: string; input: unknown; expected: unknown }
const cases: Case[] = []
const add = (name: string, kind: string, input: unknown, expected: unknown) => cases.push({ name, kind, input, expected })

// ── builders (wire shapes) ──────────────────────────────────────────────────
const P = (x: number, y: number, p = 0.5, t = 0.1): Pt => [x, y, p, t]
const stroke = (id: string, pts: Pt[], o: Partial<Extract<OpData, { op: 'stroke' }>> = {}) => ({ id, data: { op: 'stroke', tool: 'pencil', seed: 1000, size: 2, pts, ...o } as OpData, active: true })
const eraser = (id: string, pts: Pt[], size = 2, opacity?: number, only?: string[]) => stroke(id, pts, { tool: 'eraser', size, ...(opacity != null ? { opacity } : {}), ...(only ? { only } : {}) })
const image = (id: string, hash: string, rect: [number, number, number, number]) => ({ id, data: { op: 'image', hash, rect } as OpData, active: true })
const move = (id: string, poly: Poly, xf: Partial<Xf>, o: { copy?: boolean; only?: string[]; ids?: string[] } = {}) => ({ id, data: { op: 'move', poly, xf: { dx: 0, dy: 0, s: 1, cx: 0, cy: 0, ...xf }, ...o } as OpData, active: true })
const del = (id: string, poly: Poly, o: { only?: string[]; ids?: string[] } = {}) => ({ id, data: { op: 'delete', poly, ...o } as OpData, active: true })
const off = <T extends { active: boolean }>(e: T): T => ({ ...e, active: false })
const rect = (x0: number, y0: number, x1: number, y1: number): Poly => [[x0, y0], [x1, y0], [x1, y1], [x0, y1]]

type WEv = { id: string; data: OpData; active: boolean }
// wire-shaped event -> in-memory Ev (mirror of oplog.ts toEv, which is not exported)
function toEv(w: WEv): Ev {
  const d = w.data as any
  const { id, active } = w
  switch (d.op) {
    case 'stroke': return { type: 'stroke', id, tool: d.tool, seed: d.seed, size: d.size, op: d.opacity, only: d.only, pts: d.pts, active }
    case 'image': return { type: 'image', id, hash: d.hash, rect: d.rect, active }
    case 'move': return { type: 'move', id, poly: d.poly, xf: d.xf, copy: d.copy, only: d.only, ids: d.ids, active }
    default: return { type: 'delete', id, poly: d.poly, only: d.only, ids: d.ids, active }
  }
}
const ALL: Layers = { pencil: true, pen: true, image: true }

// ── fold cases ──────────────────────────────────────────────────────────────
function foldCase(name: string, evs: WEv[], extra: WEv[] = []) {
  add(name, 'fold', { evs, extra }, fold(evs.map(toEv), extra.map(toEv)))
}
const line1 = stroke('a', [P(0, 10.3), P(40, 12.7, 0.8, 0.2)], { seed: 4000000000 })
const line2 = stroke('b', [P(5, 30.1), P(15, 35.9), P(25, 31.7), P(35, 38.2)], { tool: 'pen', seed: 77, size: 3, opacity: 0.6 })

foldCase('fold: strokes and image append in order', [line1, line2, image('im', 'abc123', [10, 50, 20, 10])])
foldCase('fold: inactive events skipped', [off(line1), line2, image('im', 'h', [0, 0, 1, 1])])
foldCase('fold: extra events appended', [line1], [line2])
foldCase('fold: lasso move whole stroke (no cut)', [line1, move('m1', rect(-5, 0, 45, 20), { dx: 3.5, dy: -2.25, s: 1.5, cx: 20, cy: 11 })])
foldCase('fold: lasso move cuts stroke partially', [line1, move('m1', rect(10, 0, 30, 20), { dx: 100, dy: 50 })])
foldCase('fold: lasso move copy keeps original', [line1, move('m1', rect(-5, 0, 45, 20), { dx: 0, dy: 40.5, s: 0.5, cx: 0, cy: 0 }, { copy: true })])
foldCase('fold: copy of partial lasso keeps original whole', [line1, move('m1', rect(10, 0, 30, 20), { dx: 0, dy: 40 }, { copy: true })])
foldCase('fold: move scoped by ids', [line1, line2, move('m1', rect(-5, 0, 45, 45), { dx: 10, dy: 10 }, { ids: ['b'] })])
foldCase('fold: move with only layer', [line1, line2, move('m1', rect(-5, 0, 45, 45), { dx: 10, dy: 10 }, { only: ['pen'] })])
foldCase('fold: delete whole', [line1, line2, del('d1', rect(-5, 0, 45, 20))])
foldCase('fold: delete cuts stroke (partial lasso)', [line1, del('d1', rect(10, 0, 30, 20))])
foldCase('fold: delete with ids + only', [line1, line2, del('d1', rect(-5, 0, 45, 45), { ids: ['a', 'b'], only: ['pencil'] })])
foldCase('fold: lasso crossing a stroke twice (zigzag cut)', [stroke('z', [P(0, 0), P(10, 20), P(20, 0), P(30, 20), P(40, 0)]), move('m1', rect(5, 5, 35, 15), { dx: 0, dy: 100, s: 2, cx: 20, cy: 10 })])
foldCase('fold: single point stroke in / out', [stroke('dot1', [P(5, 5)]), stroke('dot2', [P(50, 50)]), move('m1', rect(0, 0, 10, 10), { dx: 1, dy: 1 })])
foldCase('fold: image moved when centre in poly', [image('im', 'h1', [10, 10, 20, 10]), image('in2', 'h2', [100, 10, 20, 10]), move('m1', rect(0, 0, 25, 25), { dx: 5, dy: 5, s: 2, cx: 20, cy: 15 })])
foldCase('fold: image centre outside poly stays (even if overlapping)', [image('im', 'h1', [10, 10, 20, 10]), move('m1', rect(0, 0, 18, 25), { dx: 5, dy: 5 })])
foldCase('fold: image deleted / copied', [image('im', 'h1', [10, 10, 20, 10]), image('im2', 'h2', [60, 10, 20, 10]), del('d1', rect(0, 0, 30, 30)), move('m1', rect(50, 0, 90, 30), { dx: 0, dy: 40 }, { copy: true })])
foldCase('fold: moved twice, piece ids and seeds', [line1, move('m1', rect(10, 0, 30, 20), { dx: 100, dy: 50 }), move('m2', rect(100, 40, 140, 80), { dx: -20, dy: 5, s: 0.75, cx: 120, cy: 60 })])
foldCase('fold: seed wraps past 2^32', [stroke('w', [P(0, 0), P(20, 20)], { seed: 4294967000 }), move('m1', rect(-1, -1, 30, 30), { dx: 1, dy: 1 })])
foldCase('fold: several pieces from one stroke share pieceNo counter', [stroke('z', [P(0, 0), P(10, 20), P(20, 0), P(30, 20), P(40, 0)], { seed: 5 }), stroke('y', [P(0, 8), P(40, 9)], { seed: 6 }), move('m1', rect(5, 5, 35, 15), { dx: 0, dy: 100 })])
foldCase('fold: undone (inactive) move has no effect', [line1, off(move('m1', rect(-5, 0, 45, 20), { dx: 9, dy: 9 }))])

// erasers
const base = stroke('a', [P(0, 10.3), P(40, 10.3)], { seed: 9, size: 2 })
const wipe = (id: string, op?: number, only?: string[]) => eraser(id, [P(19, 10), P(21, 10)], 2, op, only)
foldCase('fold: eraser items survive, move drops erased part', [base, wipe('e1'), move('m1', rect(-5, 0, 45, 20), { dx: 0, dy: 30 })])
foldCase('fold: eraser BEFORE stroke does not count', [wipe('e1'), base, move('m1', rect(-5, 0, 45, 20), { dx: 0, dy: 30 })])
foldCase('fold: partially transparent eraser x2 stays, x3 erases', [base, wipe('e1', 0.5), wipe('e2', 0.5), move('m1', rect(-5, 0, 45, 20), { dx: 0, dy: 30 })])
foldCase('fold: partially transparent eraser x3 erases', [base, wipe('e1', 0.5), wipe('e2', 0.5), wipe('e3', 0.5), move('m1', rect(-5, 0, 45, 20), { dx: 0, dy: 30 })])
foldCase('fold: eraser scoped to other layer ignored', [base, wipe('e1', undefined, ['pen']), move('m1', rect(-5, 0, 45, 20), { dx: 0, dy: 30 })])
foldCase('fold: erased then lasso cuts then deleted', [base, wipe('e1'), del('d1', rect(30, 0, 45, 20)), move('m1', rect(-5, 0, 22, 20), { dx: 0, dy: 60, s: 2, cx: 0, cy: 10 })])
foldCase('fold: erased stroke moved twice (moved piece lands above erasers)', [base, wipe('e1'), move('m1', rect(-5, 0, 45, 20), { dx: 0, dy: 30 }), move('m2', rect(-5, 25, 45, 45), { dx: 5, dy: 5 })])
foldCase('fold: eraser wipes entire stroke, lone speck dropped', [stroke('s', [P(10, 10), P(12, 10)]), eraser('e', [P(5, 10), P(20, 10)], 3), move('m1', rect(0, 0, 30, 30), { dx: 1, dy: 1 })])
foldCase('fold: erased copy keeps original, copy drops erased', [base, wipe('e1'), move('m1', rect(-5, 0, 45, 20), { dx: 0, dy: 30 }, { copy: true })])

// ── piecesIn ────────────────────────────────────────────────────────────────
function piecesCase(name: string, evs: WEv[], poly: Poly, layers: Layers = ALL) {
  const items = fold(evs.map(toEv))
  add(name, 'pieces', { evs, poly, layers }, { items, ...piecesIn(items, poly, layers), bbox: bboxOf(items) })
}
piecesCase('pieces: stroke cut by lasso + image by centre', [line1, line2, image('im', 'h', [10, 50, 20, 10])], rect(10, 0, 30, 60))
piecesCase('pieces: hidden layer not selectable', [line1, line2, image('im', 'h', [10, 50, 20, 10])], rect(-5, 0, 45, 60), { pencil: false, pen: true, image: true })
piecesCase('pieces: erased parts excluded', [base, wipe('e1'), line2], rect(-5, 0, 45, 45))
piecesCase('pieces: nothing selected', [line1], rect(100, 100, 110, 110))
piecesCase('pieces: only-pen eraser, pencil layer hidden', [base, wipe('e1', undefined, ['pen'])], rect(-5, 0, 45, 45), { pencil: true, pen: false, image: false })

// ── primitives ──────────────────────────────────────────────────────────────
const tri: Poly = [[0, 0], [30, 5], [10, 40]]
add('pip: points vs triangle', 'pip', { poly: tri, pts: [[10, 10], [29, 5.5], [1, 30], [15, 20], [-1, -1], [10, 40.5]] }, [[10, 10], [29, 5.5], [1, 30], [15, 20], [-1, -1], [10, 40.5]].map(([x, y]) => pip(tri, x, y)))
const sp = [P(-10, 5, 0.2, 0.9), P(15, 25, 1, 0.1), P(40, -5, 0.4, 0.5)]
add('splitByPoly: pressure/tilt interpolated at cuts', 'split', { pts: sp, poly: tri }, splitByPoly(sp, tri))
const f: Xf = { dx: 3, dy: -4.5, s: 1.25, cx: 7, cy: 9 }
add('xf: poly, stroke, image', 'xf', { xf: f, poly: tri, items: [fold([toEv(line2)])[0], fold([toEv(image('im', 'h', [10, 50, 20, 10]))])[0]] }, { poly: xfPoly(tri, f), items: [xfItem(fold([toEv(line2)])[0], f), xfItem(fold([toEv(image('im', 'h', [10, 50, 20, 10]))])[0], f)] })

// ── simplifyPoly ────────────────────────────────────────────────────────────
function simp(name: string, pts: Poly, tol: number) { add(name, 'simplify', { pts, tol }, simplifyPoly(pts, tol)) }
simp('simplify: collinear points collapse', [[0, 0], [1, 0.001], [2, -0.001], [3, 0], [10, 0]], 0.5)
simp('simplify: corner survives', [[0, 0], [5, 0.1], [10, 0], [10.1, 5], [10, 10]], 0.5)
simp('simplify: noisy circle', Array.from({ length: 40 }, (_, i) => [50 + 30 * Math.cos(i / 6.3) + (i % 3) * 0.17, 50 + 30 * Math.sin(i / 6.3) - (i % 2) * 0.11] as [number, number]), 1.5)
simp('simplify: fewer than 3 points unchanged', [[0, 0], [5, 5]], 1)
simp('simplify: coincident ends', [[0, 0], [10, 10], [0, 0]], 1)

// ── oplog scenarios ─────────────────────────────────────────────────────────
type Step =
  | { do: 'add'; data: OpData }
  | { do: 'confirm'; cid: string; eid: string; t: number }
  | { do: 'merge'; records: WireRecord[] }
  | { do: 'undo' } | { do: 'redo' }
  | { do: 'adopt'; id: string }
// deterministic clock + cids (the Swift harness uses the same: now starts 1000 and +1 per add; cids c1, c2, …)
let clock = 1000
let cidN = 0
Date.now = () => ++clock
;(globalThis.crypto as any).randomUUID = () => `c${++cidN}`
const S = (pts: Pt[] = [P(0, 0), P(5, 5)], extra: object = {}): OpData => ({ op: 'stroke', tool: 'pencil', seed: 1, size: 2, pts, ...extra }) as OpData
const rec = (event_id: string, device_id: string, recorded_at: number, data: any): WireRecord => ({ event_id, device_id, recorded_at, data })

function scenario(name: string, me: string, steps: Step[], initial: Op[] = []) {
  clock = 1000
  cidN = 0
  const log = initial.length ? MonthLog.restore('2026-01', me, initial) : new MonthLog('2026-01', me)
  const rets: unknown[] = []
  for (const s of steps) {
    switch (s.do) {
      case 'add': rets.push(log.add(s.data).cid); break
      case 'confirm': log.confirm(s.cid, s.eid, s.t); rets.push(null); break
      case 'merge': rets.push(log.merge(s.records)); break
      case 'undo': rets.push(log.undo()?.cid ?? null); break
      case 'redo': rets.push(log.redo()?.cid ?? null); break
      case 'adopt': log.adoptDevice(s.id); rets.push(null); break
    }
  }
  const events = log.events().map((e) => ({ ...toData(e), id: e.id, active: e.active }))
  add(name, 'oplog', { me, initial, steps }, {
    rets,
    ops: log.ops,
    wire: log.ops.map(toWire),
    events,
    pending: log.pending().map((o) => o.cid),
    canUndo: log.canUndo,
    canRedo: log.canRedo,
    hasContent: log.hasContent,
    version: log.version,
    me: log.me,
  })
}

scenario('oplog: add pending, confirm re-sorts', 'devA', [
  { do: 'add', data: S() }, { do: 'add', data: S([P(1, 1), P(2, 2)]) },
  { do: 'confirm', cid: 'c2', eid: 'aa02', t: 500 }, { do: 'confirm', cid: 'c1', eid: 'aa01', t: 900 }, { do: 'confirm', cid: 'c1', eid: 'zz', t: 1 },
])
scenario('oplog: undo / redo single device', 'devA', [
  { do: 'add', data: S() }, { do: 'add', data: S() }, { do: 'undo' }, { do: 'undo' }, { do: 'undo' }, { do: 'redo' }, { do: 'redo' }, { do: 'redo' },
])
scenario('oplog: redo chain ends on new drawing', 'devA', [
  { do: 'add', data: S() }, { do: 'add', data: S() }, { do: 'undo' }, { do: 'undo' }, { do: 'redo' }, { do: 'add', data: S() }, { do: 'redo' },
])
scenario('oplog: undo then new drawing, canUndo/canRedo', 'devA', [{ do: 'add', data: S() }, { do: 'undo' }, { do: 'add', data: S() }])
scenario('oplog: per-author undo skips other devices', 'devA', [
  { do: 'add', data: S() },
  { do: 'merge', records: [rec('e1', 'devB', 5000, { ...S(), cid: 'b1' }), rec('e2', 'devB', 5001, { ...S(), cid: 'b2' })] },
  { do: 'undo' }, { do: 'undo' }, { do: 'redo' },
])
scenario('oplog: remote undo of my stroke applies, my redo not offered', 'devA', [
  { do: 'add', data: S() },
  { do: 'confirm', cid: 'c1', eid: 'e1', t: 3000 },
  { do: 'merge', records: [rec('e2', 'devB', 3001, { op: 'undo', target: 'c1', cid: 'b9' })] },
])
scenario('oplog: merge stamps pending op with same cid', 'devA', [
  { do: 'add', data: S() }, { do: 'add', data: S([P(9, 9), P(8, 8)]) },
  { do: 'merge', records: [rec('s1', 'devA', 7000, { ...S(), cid: 'c1' }), rec('s1', 'devA', 7000, { ...S(), cid: 'c1' })] },
  { do: 'merge', records: [rec('s1', 'devA', 7000, { ...S(), cid: 'c1' })] },
])
scenario('oplog: merge order by t then eid, pending last, unknown ignored, cid fallback', 'devA', [
  { do: 'add', data: S() },
  { do: 'merge', records: [
    rec('b3', 'devB', 100, { ...S(), cid: 'x3' }),
    rec('a1', 'devB', 100, { ...S(), cid: 'x1' }),
    rec('c9', 'devB', 50, S([P(3, 3), P(4, 4)])),
    rec('d1', 'devB', 60, { op: 'sparkle', cid: 'x9' }),
    rec('d2', 'devB', 70, { cid: 'x10' }),
    rec('e1', 'devC', 100, { op: 'delete', poly: rect(0, 0, 1, 1), ids: ['x1'], cid: 'x4' }),
  ] },
])
scenario('oplog: merge of nothing new returns false and keeps version', 'devA', [
  { do: 'merge', records: [] }, { do: 'merge', records: [rec('q', 'devB', 1, { op: 'nope' })] },
])
scenario('oplog: adoptDevice claims earlier ops', 'local', [
  { do: 'add', data: S() }, { do: 'adopt', id: 'local' }, { do: 'adopt', id: 'devA' }, { do: 'add', data: S() }, { do: 'undo' },
  { do: 'merge', records: [rec('r1', 'devA', 9000, { ...S(), cid: 'r' })] },
])
scenario('oplog: move/delete/image ops and undo of a move', 'devA', [
  { do: 'add', data: S([P(0, 10), P(40, 12)]) },
  { do: 'add', data: { op: 'move', poly: rect(-1, 0, 50, 30), xf: { dx: 1, dy: 2, s: 1.5, cx: 3, cy: 4 }, copy: true, only: ['pencil'], ids: ['c1'] } },
  { do: 'add', data: { op: 'delete', poly: rect(0, 0, 3, 3), ids: ['c1'] } },
  { do: 'add', data: { op: 'image', hash: 'deadbeef', rect: [1, 2, 3, 4] } },
  { do: 'undo' }, { do: 'undo' }, { do: 'redo' },
])
scenario('oplog: undo of unknown / non-drawable targets ignored', 'devA', [
  { do: 'add', data: S() },
  { do: 'merge', records: [rec('u1', 'devA', 10, { op: 'undo', target: 'nope', cid: 'u1c' }), rec('u2', 'devA', 11, { op: 'undo', target: 'u1c', cid: 'u2c' })] },
])
scenario('oplog: restore from cache (pending after confirmed)', 'devA', [{ do: 'undo' }, { do: 'redo' }], [
  { cid: 'p', dev: 'devA', t: 5, data: S() },
  { cid: 'k2', dev: 'devB', t: 20, eid: 'e2', data: S() },
  { cid: 'k1', dev: 'devA', t: 20, eid: 'e1', data: S([P(1, 2, 0.3, 0.4)], { opacity: 0.25, only: ['pen'] }) },
] as Op[])
scenario('oplog: hasContent false after undoing everything', 'devA', [{ do: 'add', data: S() }, { do: 'undo' }])

// ── wire codec ──────────────────────────────────────────────────────────────
const wires: any[] = [
  { op: 'stroke', tool: 'pen', seed: 1234.0, size: 3.5, opacity: 0.5, only: ['pen', 'image'], pts: [[1, 2, 0.5, 0.25], [3, 4, 0.6, 0.3]], cid: 'w1' },
  { op: 'stroke', tool: 'eraser', seed: 4294967295, size: 8, pts: [[1, 2, 1, 0]], cid: 'w2' },
  { op: 'image', hash: 'abc', rect: [1, 2, 3.5, 4], cid: 'w3' },
  { op: 'move', poly: rect(0, 0, 5, 5), xf: { dx: 1, dy: 2, s: 3, cx: 4, cy: 5 }, copy: true, only: ['pencil'], ids: ['a', 'b~m1i0'], cid: 'w4' },
  { op: 'move', poly: rect(0, 0, 5, 5), xf: { dx: 1, dy: 2, s: 3, cx: 4, cy: 5 }, copy: false },
  { op: 'delete', poly: rect(0, 0, 5, 5), ids: ['a'], cid: 'w5' },
  { op: 'undo', target: 'w1', cid: 'w6' },
  { op: 'redo', target: 'w1', cid: 'w7' },
  { op: 'sparkle', cid: 'w8' },
]
for (const w of wires) {
  // expected: re-encoded wire (cid reattached) or null for ignored ops
  const known = ['stroke', 'image', 'move', 'delete', 'undo', 'redo'].includes(w.op)
  let out: unknown = null
  if (known) {
    const { cid, ...data } = w
    // normalise exactly as the web client would round-trip it: data -> Ev -> toData for drawables
    const d = data as OpData
    out = d.op === 'undo' || d.op === 'redo' ? { ...d, cid: cid ?? 'none' } : { ...toData(toEv({ id: 'x', active: true, data: d })), cid: cid ?? 'none' }
  }
  add(`wire: ${w.op} ${w.cid ?? '(no cid)'}`, 'wire', { wire: w }, out)
}

writeFileSync(new URL('./vectors.json', import.meta.url), JSON.stringify({ version: 1, tolerance: 1e-9, cases }, null, 1) + '\n')
console.log(`wrote ${cases.length} cases`)
