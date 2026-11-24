import { describe, expect, test } from 'bun:test'
import { MonthLog, type OpData, type WireRecord } from '../src/canvas/oplog'

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

describe('ink_fold: a fold is the base of the canvas and covers the ops it supersedes', () => {
  const recs = (eid: string, dev: string, seq: number, t: number, data: Record<string, unknown>): WireRecord => ({ event_id: eid, device_id: dev, seq, recorded_at: t, data })
  const stroke = (cid: string, y: number) => ({ op: 'stroke', cid, tool: 'pen', seed: 1, size: 5, pts: [[0, y, 0.5, 0], [50, y, 0.5, 0]] })
  const item = (id: string, y: number) => ({ id, tool: 'pen', seed: 1, size: 5, pts: [[0, y, 0.5, 0], [50, y, 0.5, 0]] })
  const fold = (items: unknown[], upTo: Record<string, number>, extra: unknown[] = []) => ({
    op: 'ink_fold', cid: 'f1', items, supersedes: [{ select: 'ink', period: '2026-W10', up_to: upTo }, ...extra],
  })
  const ids = (l: MonthLog) => l.events().filter((e) => e.active !== false).map((e) => e.id)

  test('ops covered by the fold are replaced by its items; ops above the bound fold on top', () => {
    const l = new MonthLog('2026-W10', 'dev-a')
    l.merge([
      recs('e1', 'dev-a', 1, 1, stroke('a', 0)),
      recs('e2', 'dev-a', 2, 2, stroke('b', 10)),
      recs('e3', 'dev-a', 3, 3, { op: 'undo', target: 'b', cid: 'u' }),
      recs('e4', 'dev-a', 4, 4, fold([item('a', 0)], { 'dev-a': 3 })), // b was undone, baked out of the fold
      recs('e5', 'dev-a', 5, 5, stroke('late', 20)),
    ])
    expect(ids(l)).toEqual(['a', 'late'])
  })

  test('a peer that only has the fold (the ops were collected) draws the same canvas', () => {
    const l = new MonthLog('2026-W10', 'dev-b')
    l.merge([recs('e4', 'dev-a', 4, 4, fold([item('a', 0), item('c', 30)], { 'dev-a': 3 })), recs('e5', 'dev-a', 5, 5, stroke('late', 20))])
    expect(ids(l)).toEqual(['a', 'c', 'late'])
  })

  test('a later undo can still remove a folded item', () => {
    const l = new MonthLog('2026-W10', 'dev-b')
    l.merge([recs('e4', 'dev-a', 4, 4, fold([item('a', 0), item('c', 30)], { 'dev-a': 3 })), recs('e6', 'dev-a', 6, 6, { op: 'undo', target: 'c', cid: 'u2' })])
    expect(ids(l)).toEqual(['a'])
  })

  test('the latest fold wins; an op the old fold covered is not resurrected', () => {
    const l = new MonthLog('2026-W10', 'dev-a')
    l.merge([
      recs('e1', 'dev-a', 1, 1, stroke('a', 0)),
      recs('e2', 'dev-a', 2, 2, fold([item('a', 0)], { 'dev-a': 1 })),
      recs('e3', 'dev-a', 3, 3, stroke('late', 20)),
      recs('e4', 'dev-a', 4, 4, fold([item('a', 0), item('late', 20)], { 'dev-a': 3 }, [{ id: 'e2' }])),
    ])
    expect(ids(l)).toEqual(['a', 'late'])
  })

  test('without a fold nothing changes, and ops without a seq (still pending) are never treated as covered', () => {
    const l = new MonthLog('2026-W10', 'dev-a')
    l.merge([recs('e2', 'dev-a', 2, 2, fold([item('a', 0)], { 'dev-a': 99 }))])
    l.add({ op: 'stroke', tool: 'pen', seed: 2, size: 5, pts: [[0, 5, 0.5, 0]] })
    expect(l.events().filter((e) => e.active !== false).length).toBe(2)
    expect(l.hasContent).toBe(true)
  })
})
