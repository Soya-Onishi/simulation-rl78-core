import { parseProbeValue } from '../api/client'
import type { ProbeKind, ProbeView } from '../types'
import { probeKey } from '../types'

import { RETAIN_NS } from '../timeWindow'

export { RETAIN_NS }

export interface Series {
  kind: ProbeKind
  /** Virtual time in ns, ascending. */
  t: number[]
  /** digital: 0=HiZ 1=High 2=Low, uart: data word, analog: millivolts. */
  v: number[]
  min: number
  max: number
}

/**
 * Mutable per-probe sample history kept outside React state so that high-rate
 * SSE batches do not trigger a re-render each. Subscribers are notified at most
 * every NOTIFY_MS.
 */
const NOTIFY_MS = 100

export class ProbeStore {
  private series = new Map<string, Series>()
  private listeners = new Set<() => void>()
  private timer: ReturnType<typeof setTimeout> | null = null
  private version = 0

  get(key: string): Series | undefined {
    return this.series.get(key)
  }

  getVersion = (): number => this.version

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn)
    return () => {
      this.listeners.delete(fn)
    }
  }

  clear(): void {
    this.series.clear()
    this.touch()
  }

  remove(key: string): void {
    if (this.series.delete(key)) this.touch()
  }

  /** Drop series that are not in `keep`. */
  retainOnly(keep: Set<string>): void {
    let changed = false
    for (const key of [...this.series.keys()]) {
      if (!keep.has(key)) {
        this.series.delete(key)
        changed = true
      }
    }
    if (changed) this.touch()
  }

  append(samples: ProbeView[]): void {
    for (const s of samples) {
      const key = probeKey(s.board, s.probe)
      let ser = this.series.get(key)
      if (!ser) {
        ser = { kind: s.kind, t: [], v: [], min: Infinity, max: -Infinity }
        this.series.set(key, ser)
      }
      const value = parseProbeValue(s.kind, s.value)
      const t = s.virtual_time_ns
      let last = ser.t.length - 1
      // Reset / rewind: drop this series so post-reset samples are not ignored.
      if (last >= 0 && t < ser.t[last]) {
        ser.t.length = 0
        ser.v.length = 0
        ser.min = Infinity
        ser.max = -Infinity
        last = -1
      }
      // Digital/analog coarse samples collapse same-timestamp updates.
      // UART keeps every word, even when timestamps collide.
      if (last >= 0 && t === ser.t[last] && ser.kind !== 'uart') {
        ser.v[last] = value
      } else {
        ser.t.push(t)
        ser.v.push(value)
      }
      if (value < ser.min) ser.min = value
      if (value > ser.max) ser.max = value
      this.trim(ser, t)
    }
    this.touch()
  }

  private trim(ser: Series, latest: number): void {
    const limit = latest - RETAIN_NS
    if (ser.t.length < 2 || ser.t[0] >= limit) return
    // Keep one sample at/before the limit so the line still has a start value.
    let lo = 0
    let hi = ser.t.length
    while (lo < hi) {
      const mid = (lo + hi) >> 1
      if (ser.t[mid] < limit) lo = mid + 1
      else hi = mid
    }
    const cut = Math.max(0, lo - 1)
    // Amortise the splice: only trim once a decent chunk is stale.
    if (cut < 256) return
    ser.t.splice(0, cut)
    ser.v.splice(0, cut)
    let min = Infinity
    let max = -Infinity
    for (const x of ser.v) {
      if (x < min) min = x
      if (x > max) max = x
    }
    ser.min = min
    ser.max = max
  }

  private touch(): void {
    if (this.timer) return
    this.timer = setTimeout(() => {
      this.timer = null
      this.version++
      for (const fn of this.listeners) fn()
    }, NOTIFY_MS)
  }
}

/** Index of the last sample with t[i] <= time, or -1. */
export function lastAtOrBefore(t: number[], time: number): number {
  let lo = 0
  let hi = t.length
  while (lo < hi) {
    const mid = (lo + hi) >> 1
    if (t[mid] <= time) lo = mid + 1
    else hi = mid
  }
  return lo - 1
}
