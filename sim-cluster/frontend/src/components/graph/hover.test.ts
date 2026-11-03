import { describe, expect, it } from 'vitest'
import type { Series } from '../../hooks/probeStore'
import { hoverAnalogDigital } from './hover'

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
})
