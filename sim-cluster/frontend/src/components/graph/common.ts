import { useEffect, useRef, useState } from 'react'

/** Left gutter inside every lane plot (uPlot y axis width; blank for canvas lanes). */
export const PAD_L = 44
export const PAD_R = 8
export const DIGITAL_H = 36
export const UART_H = 36
export const ANALOG_H = 96

/** Fill the region where virtual time is negative with a gray background. */
export function fillPreZero(
  ctx: CanvasRenderingContext2D,
  t0: number,
  t1: number,
  x0: number,
  x1: number,
  h: number,
) {
  if (t0 >= 0 || t1 <= t0) return
  const span = t1 - t0
  const zeroX = t1 <= 0 ? x1 : x0 + ((0 - t0) / span) * (x1 - x0)
  ctx.fillStyle = '#e8e8e8'
  ctx.fillRect(x0, 0, Math.max(0, zeroX - x0), h)
}

/** Stable colour derived from `${board}/${probe}`. */
export function colorFor(key: string): string {
  let h = 2166136261
  for (let i = 0; i < key.length; i++) {
    h ^= key.charCodeAt(i)
    h = Math.imul(h, 16777619)
  }
  const hue = (h >>> 0) % 360
  return `hsl(${hue}, 65%, 38%)`
}

export function formatUart(value: number): string {
  const hex = value.toString(16).toUpperCase().padStart(2, '0')
  if (value >= 0x20 && value <= 0x7e) return `${hex}(${String.fromCharCode(value)})`
  return hex
}

/** Tick label for a time axis with the given tick spacing (ns). */
export function formatTick(ns: number, stepNs: number): string {
  if (stepNs >= 1e9) {
    const digits = Math.max(0, Math.ceil(-Math.log10(stepNs / 1e9) - 1e-9))
    return `${(ns / 1e9).toFixed(digits)}s`
  }
  if (stepNs >= 1e6) {
    const digits = Math.max(0, Math.ceil(-Math.log10(stepNs / 1e6) - 1e-9))
    return `${(ns / 1e6).toFixed(digits)}ms`
  }
  if (stepNs >= 1e3) {
    const digits = Math.max(0, Math.ceil(-Math.log10(stepNs / 1e3) - 1e-9))
    return `${(ns / 1e3).toFixed(digits)}µs`
  }
  return `${Math.round(ns)}ns`
}

/** Pick a 1/2/5 × 10^k tick step giving roughly `target` ticks across `span`. */
export function niceStep(span: number, target = 8): number {
  const raw = span / target
  const pow = 10 ** Math.floor(Math.log10(raw))
  for (const m of [1, 2, 5, 10]) {
    if (m * pow >= raw) return m * pow
  }
  return 10 * pow
}

export interface Size {
  w: number
  h: number
}

/** Tracks the pixel size of an element. */
export function useElementSize<T extends HTMLElement>() {
  const ref = useRef<T>(null)
  const [size, setSize] = useState<Size>({ w: 0, h: 0 })
  useEffect(() => {
    const el = ref.current
    if (!el) return
    const update = () => setSize({ w: el.clientWidth, h: el.clientHeight })
    update()
    const ro = new ResizeObserver(update)
    ro.observe(el)
    return () => ro.disconnect()
  }, [])
  return [ref, size] as const
}

/**
 * Canvas filling its parent. The backing store is resized only when the
 * element size (or DPR) changes; `draw` may change every tick without realloc.
 */
export function useCanvasDraw(
  draw: (ctx: CanvasRenderingContext2D, w: number, h: number) => void,
) {
  const [hostRef, size] = useElementSize<HTMLDivElement>()
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const drawRef = useRef(draw)
  drawRef.current = draw

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas || size.w === 0 || size.h === 0) return
    const dpr = window.devicePixelRatio || 1
    const bw = Math.round(size.w * dpr)
    const bh = Math.round(size.h * dpr)
    if (canvas.width !== bw || canvas.height !== bh) {
      canvas.width = bw
      canvas.height = bh
    }
  }, [size])

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas || size.w === 0 || size.h === 0) return
    const dpr = window.devicePixelRatio || 1
    const ctx = canvas.getContext('2d')
    if (!ctx) return
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
    ctx.clearRect(0, 0, size.w, size.h)
    drawRef.current(ctx, size.w, size.h)
  }, [draw, size])
  return [hostRef, canvasRef] as const
}
