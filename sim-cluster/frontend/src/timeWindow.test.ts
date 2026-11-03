import { describe, expect, it } from 'vitest'
import {
  advanceLiveEnd,
  clampViewEnd,
  formatViewTime,
  RETAIN_NS,
  TICK_STEPS,
  TICKS_PER_WINDOW,
  tickStepIndexForSpan,
  viewExpired,
  windowNsForTick,
} from './timeWindow'

describe('timeWindow', () => {
  it('uses 10 ticks per window and a 10µs minimum tick', () => {
    expect(TICKS_PER_WINDOW).toBe(10)
    expect(TICK_STEPS[0].tickNs).toBe(10e3)
    expect(TICK_STEPS[0].label).toBe('10µs')
    // Label is the grid duration; window is 10 grids.
    expect(windowNsForTick(100e6)).toBe(1e9)
  })

  it('advances the live end only after a full tick is measured', () => {
    const tick = 10e6
    expect(advanceLiveEnd(0, 9e6, tick)).toBe(0)
    expect(advanceLiveEnd(0, 10e6, tick)).toBe(10e6)
    expect(advanceLiveEnd(0, 35e6, tick)).toBe(30e6)
  })

  it('marks a view expired once it falls behind the retain window', () => {
    const now = RETAIN_NS + 50
    expect(viewExpired(40, now)).toBe(true)
    expect(viewExpired(now - RETAIN_NS + 1, now)).toBe(false)
  })

  it('clamps history navigation into the retain span', () => {
    const tick = 10e6
    const live = 1000e6
    expect(clampViewEnd(live + tick, live, tick)).toBe(live)
    expect(clampViewEnd(-tick, live, tick)).toBe(Math.max(0, live - RETAIN_NS + windowNsForTick(tick)))
  })

  it('formats view times for the range label', () => {
    expect(formatViewTime(0)).toBe('0ns')
    expect(formatViewTime(12.5e3)).toBe('12.5µs')
    expect(formatViewTime(1.5e9)).toBe('1.500s')
  })

  it('picks a tick step whose window matches a selected span', () => {
    const idx = tickStepIndexForSpan(1e9) // 1s window → 100ms tick
    expect(TICK_STEPS[idx].tickNs).toBe(100e6)
    expect(windowNsForTick(TICK_STEPS[idx].tickNs)).toBe(1e9)
  })
})
