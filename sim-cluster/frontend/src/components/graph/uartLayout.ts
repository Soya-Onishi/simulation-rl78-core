import { formatUart } from './common'

/** Nominal UART box duration until baud is exposed (10 bits @ 9600). */
export const UART_FRAME_NS = 1_040_000

/** Pixel gap between consecutive UART boxes so they do not look fused. */
export const UART_BOX_GAP_PX = 2

/** Minimum box width when the time span collapses under zoom. */
export const UART_MIN_BOX_PX = 14

export interface UartBox {
  index: number
  tStart: number
  value: number
  x0: number
  x1: number
}

/**
 * Lay out discrete UART word boxes.
 *
 * Each word starts at its sample time with width `UART_FRAME_NS`. When zoom
 * (or tightly spaced samples) would fuse boxes, later boxes are shifted right
 * so every word stays a separate cell (PlantUML-concise style).
 */
export function layoutUartBoxes(
  times: number[],
  values: number[],
  viewT0: number,
  viewT1: number,
  plotX0: number,
  plotX1: number,
  measureText: (text: string) => number,
): UartBox[] {
  if (times.length === 0 || viewT1 <= viewT0 || plotX1 <= plotX0) return []
  const span = viewT1 - viewT0
  const xOf = (time: number) => plotX0 + ((time - viewT0) / span) * (plotX1 - plotX0)

  const boxes: UartBox[] = []
  let cursorX = -Infinity
  for (let i = 0; i < times.length; i++) {
    const tStart = times[i]
    // Keep one box before the window so a word that starts just off-screen can show.
    if (tStart > viewT1) break
    if (i + 1 < times.length && times[i + 1] < viewT0) continue

    const label = formatUart(values[i])
    const minW = Math.max(UART_MIN_BOX_PX, measureText(label) + 8)
    let x0 = xOf(tStart)
    let x1 = xOf(tStart + UART_FRAME_NS)
    if (x1 - x0 < minW) x1 = x0 + minW
    if (x0 < cursorX) {
      x0 = cursorX
      x1 = x0 + minW
    }
    if (x1 < plotX0) {
      cursorX = x1 + UART_BOX_GAP_PX
      continue
    }
    if (x0 > plotX1) break
    boxes.push({ index: i, tStart, value: values[i], x0, x1 })
    cursorX = x1 + UART_BOX_GAP_PX
  }
  return boxes
}

export function hitUartBox(boxes: UartBox[], x: number): UartBox | null {
  for (const box of boxes) {
    if (x >= box.x0 && x < box.x1) return box
  }
  return null
}
