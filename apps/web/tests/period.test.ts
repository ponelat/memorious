import { expect, test } from 'bun:test'
import { isMonthKey, monthLabel, prevMonth, weekKey } from '../src/canvas/period'

// the same cases as crates/core/tests/ink.rs `week_keys_are_utc_iso_weeks_starting_monday`
const at = (y: number, m: number, d: number, h = 0) => new Date(Date.UTC(y, m - 1, d, h))

test('week keys are UTC ISO weeks starting Monday', () => {
  expect(weekKey(at(2026, 11, 24, 7))).toBe('2026-W48')
  expect(weekKey(at(2026, 11, 23, 0))).toBe('2026-W48')
  expect(weekKey(at(2026, 11, 22, 23))).toBe('2026-W47')
  expect(weekKey(at(2026, 1, 1, 12))).toBe('2026-W01')
  expect(weekKey(at(2025, 12, 29, 0))).toBe('2026-W01')
  expect(weekKey(at(2027, 1, 1, 0))).toBe('2026-W53')
})

test('previous period: weeks step back a week, legacy months a month', () => {
  expect(prevMonth('2026-W01')).toBe('2025-W52')
  expect(prevMonth('2026-W48')).toBe('2026-W47')
  expect(prevMonth('2026-01')).toBe('2025-12')
  expect(prevMonth('2026-11')).toBe('2026-10')
})

test('period keys: weeks and months, nothing else', () => {
  for (const ok of ['2026-W48', '2026-W01', '2026-W53', '2026-11']) expect(isMonthKey(ok)).toBe(true)
  for (const bad of ['2026-W00', '2026-W54', '2026-13', '2026-00', '2026-w48', '2026-W4', '']) expect(isMonthKey(bad)).toBe(false)
  expect(monthLabel('2026-W48')).toContain('week 48')
})
