import { lastAtOrBefore, type Series } from '../../hooks/probeStore'
import { formatViewTime } from '../../timeWindow'
import { PAD_L, PAD_R } from './common'

/** Floating dialog contents while the pointer is over a lane. */
export interface LaneHoverInfo {
  x: number
  y: number
  lines: string[]
}

export type LaneHoverHandler = (info: LaneHoverInfo | null) => void

/** Map a client X inside a lane host to virtual time, or null outside the plot. */
export function timeFromClientX(
  clientX: number,
  host: HTMLElement,
  t0: number,
  t1: number,
): number | null {
  if (t1 <= t0) return null
  const rect = host.getBoundingClientRect()
  const x = clientX - rect.left
  const x0 = PAD_L
  const x1 = rect.width - PAD_R
  if (x < x0 || x > x1) return null
  return t0 + ((x - x0) / (x1 - x0)) * (t1 - t0)
}

function digitalLabel(level: number): string {
  if (level === 1) return 'High'
  if (level === 2) return 'Low'
  return 'Hi-Z'
}

/** Sample lookup for analog / digital hover (last value at or before `time`). */
export function hoverAnalogDigital(
  series: Series | undefined,
  time: number,
  kind: 'analog' | 'digital',
): string[] | null {
  if (!series || series.t.length === 0) return null
  const i = lastAtOrBefore(series.t, time)
  if (i < 0) return null
  const t = series.t[i]
  const v = series.v[i]
  if (kind === 'analog') {
    return [`t=${formatViewTime(t)}`, `${v} mV`]
  }
  return [`t=${formatViewTime(t)}`, digitalLabel(v)]
}
