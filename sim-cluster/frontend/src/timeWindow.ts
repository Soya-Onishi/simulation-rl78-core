/** Number of tick grids that fit in one graph window. */
export const TICKS_PER_WINDOW = 10

/** Keep this much virtual-time history in the probe store. */
export const RETAIN_NS = 10 * 3600 * 1e9

/**
 * Tick (grid) widths selected by −/+.
 * Window span = tick × {@link TICKS_PER_WINDOW} (ten grids on screen).
 * Labels are the grid duration (not the full window).
 */
export const TICK_STEPS: { label: string; tickNs: number }[] = [
  { label: '10µs', tickNs: 10e3 },
  { label: '100µs', tickNs: 100e3 },
  { label: '1ms', tickNs: 1e6 },
  { label: '10ms', tickNs: 10e6 },
  { label: '100ms', tickNs: 100e6 },
  { label: '1s', tickNs: 1e9 },
  { label: '10s', tickNs: 10e9 },
  { label: '1m', tickNs: 60e9 },
  { label: '5m', tickNs: 300e9 },
  { label: '10m', tickNs: 600e9 },
  { label: '30m', tickNs: 1800e9 },
  { label: '1h', tickNs: 3600e9 },
]

/** Default −/+ index: 100ms per grid. */
export const DEFAULT_TICK_STEP_INDEX = TICK_STEPS.findIndex((s) => s.tickNs === 100e6)

export function windowNsForTick(tickNs: number): number {
  return tickNs * TICKS_PER_WINDOW
}

/** Advance a live right-edge by whole ticks once `virtualTimeNs` fills them. */
export function advanceLiveEnd(viewEndNs: number, virtualTimeNs: number, tickNs: number): number {
  if (tickNs <= 0) return viewEndNs
  let end = viewEndNs
  while (virtualTimeNs >= end + tickNs) end += tickNs
  return end
}

/** Clamp a history right-edge into the retain window ending at `liveEndNs`. */
export function clampViewEnd(viewEndNs: number, liveEndNs: number, tickNs: number): number {
  const windowNs = windowNsForTick(tickNs)
  const earliestEnd = Math.max(0, liveEndNs - RETAIN_NS + windowNs)
  return Math.min(liveEndNs, Math.max(earliestEnd, viewEndNs))
}

/** True when the visible window has fallen entirely outside the 10h retain span. */
export function viewExpired(viewEndNs: number, virtualTimeNs: number): boolean {
  return viewEndNs <= virtualTimeNs - RETAIN_NS
}

export function formatViewTime(ns: number): string {
  if (!Number.isFinite(ns)) return '—'
  const sign = ns < 0 ? '-' : ''
  const abs = Math.abs(ns)
  if (abs >= 3600e9) {
    const h = Math.floor(abs / 3600e9)
    const m = Math.floor((abs % 3600e9) / 60e9)
    const s = Math.floor((abs % 60e9) / 1e9)
    return `${sign}${h}h${String(m).padStart(2, '0')}m${String(s).padStart(2, '0')}s`
  }
  if (abs >= 60e9) {
    const m = Math.floor(abs / 60e9)
    const s = Math.floor((abs % 60e9) / 1e9)
    const ms = Math.floor((abs % 1e9) / 1e6)
    return `${sign}${m}m${String(s).padStart(2, '0')}.${String(ms).padStart(3, '0')}s`
  }
  if (abs >= 1e9) {
    return `${sign}${(abs / 1e9).toFixed(3)}s`
  }
  if (abs >= 1e6) {
    return `${sign}${(abs / 1e6).toFixed(3)}ms`
  }
  if (abs >= 1e3) {
    return `${sign}${(abs / 1e3).toFixed(1)}µs`
  }
  return `${sign}${Math.round(abs)}ns`
}
