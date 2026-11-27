import { describe, expect, test } from 'bun:test'
import { MonthLog, type OpData, type WireRecord } from '../src/canvas/oplog'
import { fold } from '../src/canvas/select'

const stroke = (seed: number): OpData => ({ op: 'stroke', tool: 'pen', seed, size: 5, pts: [[0, 0, 0.5, 0]] })
const rec = (eid: string, dev: string, t: number, data: Record<string, unknown>): WireRecord => ({ event_id: eid, device_id: dev, recorded_at: t, data })
const seeds = (l: MonthLog) => l.events().filter((e) => e.active !== false && e.type === 'stroke').map((e) => (e.type === 'stroke' ? e.seed : -1))

describe('MonthLog', () => {
  test('local ops appear immediately, in order, as pending', () => {
    const l = new MonthLog('2026-11', 'me')
    l.add(stroke(1))
    l.add(stroke(2))
    expect(seeds(l)).toEqual([1, 2])
    expect(l.pending().length).toBe(2)
  })

  test('undo and redo are ops, only on my own work, and redo survives a rebuild of the log', () => {
    const l = new MonthLog('2026-11', 'me')
    l.add(stroke(1))
    l.add(stroke(2))
    expect(l.canUndo).toBe(true)
    l.undo()
    expect(seeds(l)).toEqual([1])
    expect(l.canRedo).toBe(true)
    // a log restored from its ops (a reload) still knows what can be redone
    const again = MonthLog.restore('2026-11', 'me', l.ops)
    expect(again.canRedo).toBe(true)
    again.redo()
    expect(seeds(again)).toEqual([1, 2])
    expect(again.canRedo).toBe(false)
  })

  test('a new op ends the redo chain', () => {
    const l = new MonthLog('2026-11', 'me')
    l.add(stroke(1))
    l.undo()
    l.add(stroke(2))
    expect(l.canRedo).toBe(false)
    expect(seeds(l)).toEqual([2])
  })

  test('you cannot undo somebody else\'s stroke', () => {
    const l = new MonthLog('2026-11', 'me')
    l.merge([rec('e1', 'other', 100, { op: 'stroke', cid: 'c1', tool: 'pen', seed: 7, size: 5, pts: [[0, 0, 0.5, 0]] })])
    expect(seeds(l)).toEqual([7])
    expect(l.canUndo).toBe(false)
    l.add(stroke(1))
    l.undo()
    expect(seeds(l)).toEqual([7])
  })

  test('the pushed ops are confirmed by cid, not duplicated, when they come back', () => {
    const l = new MonthLog('2026-11', 'me')
    const op = l.add(stroke(1))
    l.merge([rec('e9', 'me', 500, { ...op.data, cid: op.cid })])
    expect(l.ops.length).toBe(1)
    expect(l.pending().length).toBe(0)
    expect(l.ops[0].eid).toBe('e9')
    // the same record arriving again (a retry) changes nothing
    expect(l.merge([rec('e9', 'me', 500, { ...op.data, cid: op.cid })])).toBe(false)
    expect(l.ops.length).toBe(1)
  })

  test('canvas order is (recorded_at, event_id), whatever order ops arrived in', () => {
    const a = new MonthLog('2026-11', 'x')
    const b = new MonthLog('2026-11', 'y')
    const r1 = rec('b', 'p', 100, { op: 'stroke', cid: 'c1', tool: 'pen', seed: 1, size: 5, pts: [[0, 0, 1, 0]] })
    const r2 = rec('a', 'q', 100, { op: 'stroke', cid: 'c2', tool: 'pen', seed: 2, size: 5, pts: [[0, 0, 1, 0]] })
    const r3 = rec('c', 'p', 50, { op: 'stroke', cid: 'c3', tool: 'pen', seed: 3, size: 5, pts: [[0, 0, 1, 0]] })
    a.merge([r1, r2, r3])
    b.merge([r3, r2])
    b.merge([r1])
    expect(seeds(a)).toEqual([3, 2, 1])
    expect(seeds(b)).toEqual(seeds(a))
  })

  test('a late-arriving old op slots into place and the fold changes', () => {
    const l = new MonthLog('2026-11', 'me')
    l.merge([rec('z', 'p', 200, { op: 'stroke', cid: 'c2', tool: 'pen', seed: 2, size: 5, pts: [[0, 0, 1, 0]] })])
    l.merge([rec('y', 'q', 100, { op: 'stroke', cid: 'c1', tool: 'pen', seed: 1, size: 5, pts: [[0, 0, 1, 0]] })])
    expect(seeds(l)).toEqual([1, 2])
  })

  test('my unconfirmed ops stay after everything confirmed, until the server stamps them', () => {
    const l = new MonthLog('2026-11', 'me')
    const op = l.add(stroke(1))
    l.merge([rec('e1', 'other', Date.now() + 10_000, { op: 'stroke', cid: 'c9', tool: 'pen', seed: 9, size: 5, pts: [[0, 0, 1, 0]] })])
    expect(seeds(l)).toEqual([9, 1])
    l.confirm(op.cid, 'e2', Date.now())
    expect(l.pending().length).toBe(0)
  })

  test('unknown ops are ignored, not fatal', () => {
    const l = new MonthLog('2026-11', 'me')
    l.merge([rec('e1', 'p', 1, { op: 'hologram', cid: 'c1' }), rec('e2', 'p', 2, { cid: 'c2' })])
    expect(l.events().length).toBe(0)
  })

  test('opacity travels as `opacity` on the wire and `op` in memory', () => {
    const l = new MonthLog('2026-11', 'me')
    l.addEvent({ type: 'stroke', tool: 'pencil', seed: 1, size: 4.5, op: 0.7, pts: [[0, 0, 1, 0]] })
    const wire = l.pending()[0].data as Record<string, unknown>
    expect(wire.opacity).toBe(0.7)
    expect('op' in l.events()[0] && (l.events()[0] as { op?: number }).op).toBe(0.7)
  })

  test('selection scope, eraser layers and op ids survive the wire and reach the fold', () => {
    const l = new MonthLog('2026-11', 'me')
    const a = l.addEvent({ type: 'stroke', tool: 'eraser', seed: 1, size: 5, op: 0.4, only: ['pencil'], pts: [[0, 0, 1, 0]] })
    const m = l.addEvent({ type: 'move', poly: [[0, 0], [1, 0], [1, 1]], xf: { dx: 1, dy: 2, s: 1, cx: 0, cy: 0 }, ids: ['x', 'y'] })
    const evs = l.events()
    expect((evs[0] as { only?: string[]; id?: string }).only).toEqual(['pencil'])
    expect((evs[0] as { id?: string }).id).toBe(a.cid)
    expect((evs[1] as { ids?: string[]; id?: string }).ids).toEqual(['x', 'y'])
    expect((evs[1] as { id?: string }).id).toBe(m.cid)
    expect((m.data as { ids?: string[] }).ids).toEqual(['x', 'y'])
  })
})

describe('a legacy ink_fold op (dropped 2026-11-26) is an unknown op: it draws nothing and covers nothing', () => {
  const recs = (eid: string, dev: string, seq: number, t: number, data: Record<string, unknown>): WireRecord => ({ event_id: eid, device_id: dev, seq, recorded_at: t, data })
  const stroke = (cid: string, y: number) => ({ op: 'stroke', cid, tool: 'pen', seed: 1, size: 5, pts: [[0, y, 0.5, 0], [50, y, 0.5, 0]] })
  const item = (id: string, y: number) => ({ id, tool: 'pen', seed: 1, size: 5, pts: [[0, y, 0.5, 0], [50, y, 0.5, 0]] })
  const ids = (l: MonthLog) => l.events().filter((e) => e.active !== false).map((e) => e.id)

  test('the plain ops stay the canvas; the fold is ignored', () => {
    const l = new MonthLog('2026-W10', 'dev-a')
    l.merge([
      recs('e1', 'dev-a', 1, 1, stroke('a', 0)),
      recs('e2', 'dev-a', 2, 2, stroke('b', 10)),
      recs('e3', 'dev-a', 3, 3, { op: 'ink_fold', cid: 'f1', items: [item('x', 99)], supersedes: [{ select: 'ink', period: '2026-W10', up_to: { 'dev-a': 3 } }] }),
      recs('e4', 'dev-a', 4, 4, stroke('late', 20)),
    ])
    expect(ids(l)).toEqual(['a', 'b', 'late'])
  })
})

describe('wiping: a delete op written into an earlier period', () => {
  const rec = (eid: string, dev: string, seq: number, t: number, data: Record<string, unknown>): WireRecord => ({ event_id: eid, device_id: dev, seq, recorded_at: t, data })
  const stroke = (cid: string, y: number, tool = 'pen', seed = 1) => ({ op: 'stroke', cid, tool, seed, size: 5, pts: [[0, y, 0.5, 0], [50, y, 0.5, 0]] })
  const base = () => [rec('e1', 'dev-a', 1, 1, stroke('a', 0)), rec('e2', 'dev-a', 2, 2, stroke('b', 100)), rec('e3', 'dev-a', 3, 3, stroke('c', 200))]
  const ids = (l: MonthLog) => fold(l.events()).map((i) => i.id)

  test('the delete folds on top of the ink: what is inside the shape goes, the rest stays', () => {
    const l = new MonthLog('2026-W10', 'dev-b')
    l.merge([...base(), rec('e5', 'dev-b', 1, 5, { op: 'delete', cid: 'w1', poly: [[-10, 90], [60, 90], [60, 110], [-10, 110]] })])
    expect(ids(l)).toEqual(['a', 'c'])
  })

  test('a delete written locally (still pending, no seq) applies at once', () => {
    const l = new MonthLog('2026-W10', 'dev-b')
    l.merge(base())
    l.addEvent({ type: 'delete', poly: [[-10, 190], [60, 190], [60, 210], [-10, 210]], active: true } as never)
    expect(ids(l)).toEqual(['a', 'b'])
  })

  test('only the visible layers are touched', () => {
    const l = new MonthLog('2026-W10', 'dev-b')
    l.merge([rec('e1', 'dev-a', 1, 1, stroke('b', 100)), rec('e2', 'dev-a', 2, 2, stroke('p', 100, 'pencil', 2)), rec('e5', 'dev-b', 1, 5, { op: 'delete', cid: 'w2', only: ['pen'], poly: [[-10, 90], [60, 90], [60, 110], [-10, 110]] })])
    expect(ids(l)).toEqual(['p'])
  })
})
