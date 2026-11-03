import { useEffect, useRef } from 'react'
import uPlot from 'uplot'
import 'uplot/dist/uPlot.min.css'
import type { Series } from '../../hooks/probeStore'
import { ANALOG_H, PAD_L, PAD_R, useElementSize } from './common'

interface Props {
  series: Series | undefined
  t0: number
  t1: number
  color: string
  /** Store version; bumps whenever samples were appended. */
  version: number
}

export default function AnalogLane({ series, t0, t1, color, version }: Props) {
  const [hostRef, size] = useElementSize<HTMLDivElement>()
  const plotRef = useRef<uPlot | null>(null)
  const live = useRef({ t0, t1, series })
  live.current = { t0, t1, series }
  const lastDataVersion = useRef<number | null>(null)

  useEffect(() => {
    const host = hostRef.current
    if (!host) return
    const opts: uPlot.Options = {
      width: host.clientWidth || 300,
      height: ANALOG_H,
      padding: [4, PAD_R, 4, 0],
      legend: { show: false },
      cursor: { show: false },
      select: { show: false, left: 0, top: 0, width: 0, height: 0 },
      scales: {
        x: { time: false, range: () => [live.current.t0, live.current.t1] },
        y: {
          range: () => {
            const s = live.current.series
            if (!s || !Number.isFinite(s.min) || !Number.isFinite(s.max)) return [0, 1]
            if (s.min === s.max) return [s.min - 1, s.max + 1]
            const pad = (s.max - s.min) * 0.08
            return [s.min - pad, s.max + pad]
          },
        },
      },
      axes: [
        { show: false },
        {
          size: PAD_L,
          font: '10px sans-serif',
          stroke: '#666',
          grid: { stroke: '#eee', width: 1 },
          ticks: { stroke: '#ccc', width: 1, size: 3 },
          gap: 2,
        },
      ],
      series: [{}, { stroke: color, width: 1.5, points: { show: false } }],
    }
    const plot = new uPlot(opts, [[], []], host)
    plotRef.current = plot
    lastDataVersion.current = null
    return () => {
      plot.destroy()
      plotRef.current = null
    }
    // hostRef is stable; colour is fixed per lane (derived from the probe key)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useEffect(() => {
    if (size.w > 0) plotRef.current?.setSize({ width: size.w, height: ANALOG_H })
  }, [size.w])

  useEffect(() => {
    const plot = plotRef.current
    if (!plot) return
    if (lastDataVersion.current !== version) {
      const t = series?.t ?? []
      const v = series?.v ?? []
      plot.setData([t, v], false)
      lastDataVersion.current = version
    }
    plot.setScale('x', { min: t0, max: t1 })
  }, [series, t0, t1, version, size.w])

  return <div className="lane-plot" style={{ height: ANALOG_H }} ref={hostRef} />
}
