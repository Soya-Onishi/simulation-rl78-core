import { describe, expect, it } from 'vitest'
import {
  hitUartBox,
  layoutUartBoxes,
  UART_FRAME_NS,
  uartLabelFits,
} from './uartLayout'

describe('layoutUartBoxes', () => {
  it('places each box at the sample virtual time without packing', () => {
    const times = [0, 5e6, 10e6]
    const values = [0x41, 0x42, 0x43]
    const t0 = 0
    const t1 = 20e6
    const plotX0 = 0
    const plotX1 = 200
    const boxes = layoutUartBoxes(times, values, t0, t1, plotX0, plotX1)
    expect(boxes).toHaveLength(3)
    const xOf = (t: number) => plotX0 + ((t - t0) / (t1 - t0)) * (plotX1 - plotX0)
    expect(boxes[0].x0).toBeCloseTo(xOf(0))
    expect(boxes[1].x0).toBeCloseTo(xOf(5e6))
    expect(boxes[2].x0).toBeCloseTo(xOf(10e6))
  })

  it('clips width to the next sample so later words keep their own start time', () => {
    const boxes = layoutUartBoxes([0, 100], [0x41, 0x42], 0, 1e9, 0, 1000)
    expect(boxes).toHaveLength(2)
    expect(boxes[0].x1).toBeLessThanOrEqual(boxes[1].x0)
    expect(boxes[1].x0).toBeGreaterThan(boxes[0].x0)
  })

  it('uses the nominal frame length when the next word is farther away', () => {
    const t0 = 0
    const t1 = 10e6
    const boxes = layoutUartBoxes([0], [0x41], t0, t1, 0, 1000)
    const expectedW = (UART_FRAME_NS / (t1 - t0)) * 1000
    expect(boxes[0].x1 - boxes[0].x0).toBeCloseTo(expectedW)
  })

  it('reports that tight labels do not fit a narrow box', () => {
    expect(uartLabelFits(4, '41(A)', () => 30)).toBe(false)
    expect(uartLabelFits(40, '41(A)', () => 30)).toBe(true)
  })

  it('hit-tests the time-aligned box under the pointer', () => {
    const boxes = layoutUartBoxes([0, 5e6], [0x41, 0x42], 0, 10e6, 0, 500)
    expect(hitUartBox(boxes, boxes[0].x0 + 0.1)?.value).toBe(0x41)
    expect(hitUartBox(boxes, boxes[1].x0 + 0.1)?.value).toBe(0x42)
  })
})
