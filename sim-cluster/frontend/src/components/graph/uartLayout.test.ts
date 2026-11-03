import { describe, expect, it } from 'vitest'
import { hitUartBox, layoutUartBoxes, UART_BOX_GAP_PX, UART_MIN_BOX_PX } from './uartLayout'

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

  it('hit-tests the discrete box under the pointer', () => {
    const boxes = layoutUartBoxes([0, 1e6], [0x41, 0x42], 0, 10e6, 0, 500, () => 10)
    expect(hitUartBox(boxes, boxes[0].x0 + 1)?.value).toBe(0x41)
    expect(hitUartBox(boxes, boxes[1].x0 + 1)?.value).toBe(0x42)
    expect(hitUartBox(boxes, boxes[0].x1 + UART_BOX_GAP_PX / 2)).toBeNull()
  })
})
