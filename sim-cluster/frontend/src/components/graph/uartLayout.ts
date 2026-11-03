/** Nominal UART box duration until baud is exposed (10 bits @ 9600). */
export const UART_FRAME_NS = 1_040_000

export interface UartBox {
  index: number
  tStart: number
  value: number
  x0: number
  x1: number
}

/**
 * Lay out UART word boxes at their real virtual times.
 *
 * - Left edge is always `xOf(tStart)` (no packing / shifting).
 * - Width is `UART_FRAME_NS`, clipped so the box does not enter the next word.
 * - Callers omit in-box labels when the pixel width is too small for the text.
 */
export function layoutUartBoxes(
  times: number[],
  values: number[],
  viewT0: number,
  viewT1: number,
  plotX0: number,
  plotX1: number,
): UartBox[] {
  if (times.length === 0 || viewT1 <= viewT0 || plotX1 <= plotX0) return []
  const span = viewT1 - viewT0
  const xOf = (time: number) => plotX0 + ((time - viewT0) / span) * (plotX1 - plotX0)

  const boxes: UartBox[] = []
  for (let i = 0; i < times.length; i++) {
    const tStart = times[i]
    if (tStart > viewT1) break
    const tEnd =
      i + 1 < times.length
        ? Math.min(tStart + UART_FRAME_NS, times[i + 1])
        : tStart + UART_FRAME_NS
    if (tEnd <= viewT0) continue

    const x0 = xOf(tStart)
    const x1 = xOf(tEnd)
    if (x1 < plotX0 || x0 > plotX1) continue
    boxes.push({ index: i, tStart, value: values[i], x0, x1 })
  }
  return boxes
}

export function hitUartBox(boxes: UartBox[], x: number): UartBox | null {
  for (const box of boxes) {
    if (x >= box.x0 && x < box.x1) return box
  }
  return null
}

export function uartLabelFits(boxWidth: number, label: string, measureText: (text: string) => number): boolean {
  return measureText(label) + 6 <= boxWidth
}
