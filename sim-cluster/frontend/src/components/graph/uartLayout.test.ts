import { describe, expect, it } from 'vitest'
import {
  hitUartBox,
  layoutUartBoxes,
  UART_BOX_GAP_PX,
  UART_MAX_BOX_FRAC,
  UART_MIN_BOX_PX,
} from './uartLayout'

describe('layoutUartBoxes', () => {
  it('keeps tightly spaced words as separate boxes instead of fusing them', () => {
    const times = [15, 16, 17]
    const values = [0x41, 0x42, 0x43]
    const boxes = layoutUartBoxes(times, values, 0, 1e9, 0, 1000, () => 10)
    expect(boxes).toHaveLength(3)
    expect(boxes[0].x1).toBeLessThanOrEqual(boxes[1].x0)
    expect(boxes[1].x1).toBeLessThanOrEqual(boxes[2].x0)
    expect(boxes[1].x0 - boxes[0].x1).toBe(UART_BOX_GAP_PX)
    expect(boxes[0].x1 - boxes[0].x0).toBeGreaterThanOrEqual(UART_MIN_BOX_PX)
  })

  it('caps a single frame so fine zoom cannot cover the whole plot', () => {
    // Window is 100µs; UART_FRAME_NS is ~1ms (10× the window).
    const boxes = layoutUartBoxes([0], [0x41], 0, 100e3, 0, 1000, () => 10)
    expect(boxes).toHaveLength(1)
    expect(boxes[0].x1 - boxes[0].x0).toBeLessThanOrEqual(1000 * UART_MAX_BOX_FRAC + 1e-6)
    // Uncapped time mapping would be ~10× the plot width.
    expect(boxes[0].x1 - boxes[0].x0).toBeLessThan(1000)
  })

  it('does not let a box run into the next word', () => {
    const t0 = 0
    const t1 = 10e6
    const boxes = layoutUartBoxes([0, 2e6], [0x41, 0x42], t0, t1, 0, 1000, () => 10)
    expect(boxes).toHaveLength(2)
    expect(boxes[0].x1).toBeLessThanOrEqual(boxes[1].x0)
  })

  it('hit-tests the discrete box under the pointer', () => {
    const boxes = layoutUartBoxes([0, 1e6], [0x41, 0x42], 0, 10e6, 0, 500, () => 10)
    expect(hitUartBox(boxes, boxes[0].x0 + 1)?.value).toBe(0x41)
    expect(hitUartBox(boxes, boxes[1].x0 + 1)?.value).toBe(0x42)
    expect(hitUartBox(boxes, boxes[0].x1 + UART_BOX_GAP_PX / 2)).toBeNull()
  })
})
