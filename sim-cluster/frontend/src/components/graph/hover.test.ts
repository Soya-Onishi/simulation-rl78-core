import { describe, expect, it } from 'vitest'
import type { Series } from '../../hooks/probeStore'
import { hoverAnalogDigital, hoverUart, UART_FRAME_NS } from './hover'

describe('lane hover', () => {
  it('reports analog mV at the sample at or before the pointer time', () => {
    const series: Series = {
      kind: 'analog',
      t: [0, 1e6, 2e6],
      v: [100, 200, 300],
      min: 100,
      max: 300,
    }
    expect(hoverAnalogDigital(series, 1.5e6, 'analog')).toEqual(['t=1.000ms', '200 mV'])
  })

  it('reports UART only when the pointer is inside a frame box', () => {
    const series: Series = {
      kind: 'uart',
      t: [1e6],
      v: [0x41],
      min: 0x41,
      max: 0x41,
    }
    expect(hoverUart(series, 1e6 + UART_FRAME_NS / 2)).toEqual(['t=1.000ms', '41(A)'])
    expect(hoverUart(series, 1e6 + UART_FRAME_NS + 1)).toBeNull()
  })
})
