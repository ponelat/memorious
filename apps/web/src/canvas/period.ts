/** Canvas periods: pure date maths (no DOM), shared by the store and tested in tests/period.test.ts. Mirrors crates/core/src/ink.rs. */
const periodRe = /^\d{4}-(W(0[1-9]|[1-4]\d|5[0-3])|(0[1-9]|1[0-2]))$/
const DAY = 86400000

/** A canvas is one UTC ISO week, Monday 00:00 to Sunday 24:00, keyed `YYYY-Www` by the ISO year — the same week on every peer, whatever
 *  its time zone. (Canvases made before weekly canvases are legacy months, `YYYY-MM`; both kinds are valid period keys.) */
export function weekKey(d = new Date()): string {
  const t = new Date(Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate()))
  const day = (t.getUTCDay() + 6) % 7 // Monday = 0
  t.setUTCDate(t.getUTCDate() - day + 3) // the Thursday of this week decides the ISO year
  const isoYear = t.getUTCFullYear()
  const week = Math.floor((t.getTime() - Date.UTC(isoYear, 0, 1)) / DAY / 7) + 1
  return `${isoYear}-W${String(week).padStart(2, '0')}`
}

/** Monday 00:00 UTC of a week key. */
function weekStart(key: string): number {
  const [y, w] = key.split('-W').map(Number)
  const jan4 = Date.UTC(y, 0, 4)
  const mondayOfWeek1 = jan4 - ((new Date(jan4).getUTCDay() + 6) % 7) * DAY
  return mondayOfWeek1 + (w - 1) * 7 * DAY
}

/** The canvas you draw on now. (Named `monthKey` from when canvases were monthly; it is a week now.) */
export const monthKey = (d = new Date()) => weekKey(d)

/** The period just before `key`: the previous week, or for a legacy month the previous month. */
export function prevMonth(key: string): string {
  if (key.includes('-W')) return weekKey(new Date(weekStart(key) - DAY))
  const [y, m] = key.split('-').map(Number)
  return m === 1 ? `${y - 1}-12` : `${y}-${String(m - 1).padStart(2, '0')}`
}

export function monthLabel(key: string): string {
  if (key.includes('-W')) {
    const start = new Date(weekStart(key))
    return `week ${key.split('-W')[1]} · ${start.toLocaleDateString(undefined, { day: 'numeric', month: 'short', timeZone: 'UTC' })}`
  }
  const [y, m] = key.split('-').map(Number)
  return new Date(y, m - 1, 1).toLocaleDateString(undefined, { month: 'short', year: 'numeric' })
}

export const isMonthKey = (s: string) => periodRe.test(s)

