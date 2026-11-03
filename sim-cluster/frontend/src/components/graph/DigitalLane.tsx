import type { MouseEvent } from 'react'
import type { Series } from '../../hooks/probeStore'
import { lastAtOrBefore } from '../../hooks/probeStore'
import { DIGITAL_H, fillPreZero, PAD_L, PAD_R, useCanvasDraw } from './common'
import { hoverAnalogDigital, timeFromClientX, type LaneHoverHandler } from './hover'

interface Props {
  series: Series | undefined
  t0: number
  t1: number
  color: string
  onHover?: LaneHoverHandler
}

const MARGIN = 6

export default function DigitalLane({ series, t0, t1, color, onHover }: Props) {
  const draw = (ctx: CanvasRenderingContext2D, w: number, h: number) => {
    const x0 = PAD_L
    const x1 = w - PAD_R
    const yHigh = MARGIN
    const yLow = h - MARGIN
    const yMid = h / 2

    fillPreZero(ctx, t0, t1, x0, x1, h)

    ctx.fillStyle = '#999'
    ctx.font = '10px sans-serif'
    ctx.textBaseline = 'middle'
    ctx.textAlign = 'right'
    ctx.fillText('H', x0 - 6, yHigh)
    ctx.fillText('L', x0 - 6, yLow)

    ctx.strokeStyle = '#eee'
    ctx.lineWidth = 1
    ctx.beginPath()
    for (const y of [yHigh, yMid, yLow]) {
      ctx.moveTo(x0, y + 0.5)
      ctx.lineTo(x1, y + 0.5)
    }
    ctx.stroke()

    if (!series || series.t.length === 0) return
    const { t, v } = series
    const span = t1 - t0
    const xOf = (time: number) => x0 + ((time - t0) / span) * (x1 - x0)
    const yOf = (level: number) => (level === 1 ? yHigh : level === 2 ? yLow : yMid)

    ctx.save()
    ctx.beginPath()
    ctx.rect(x0, 0, x1 - x0, h)
    ctx.clip()
    ctx.strokeStyle = color
    ctx.lineWidth = 1.5
    ctx.lineJoin = 'miter'
    ctx.beginPath()
    let i = lastAtOrBefore(t, t0)
    if (i < 0) i = 0
    let y = yOf(v[i])
    ctx.moveTo(xOf(Math.max(t[i], t0 - span)), y)
    for (let j = i + 1; j < t.length; j++) {
      if (t[j] > t1) break
      const x = xOf(t[j])
      ctx.lineTo(x, y)
      y = yOf(v[j])
      ctx.lineTo(x, y)
    }
    ctx.lineTo(x1, y)
    ctx.stroke()
    ctx.restore()
  }

  const [hostRef, canvasRef] = useCanvasDraw(draw)

  const report = (ev: MouseEvent<HTMLDivElement>) => {
    if (!onHover) return
    const host = hostRef.current
    if (!host) return
    const time = timeFromClientX(ev.clientX, host, t0, t1)
    if (time === null) {
      onHover(null)
      return
    }
    const lines = hoverAnalogDigital(series, time, 'digital')
    onHover(lines ? { x: ev.clientX, y: ev.clientY, lines } : null)
  }

  return (
    <div
      className="lane-plot"
      style={{ height: DIGITAL_H }}
      ref={hostRef}
      onMouseMove={report}
      onMouseLeave={() => onHover?.(null)}
    >
      <canvas ref={canvasRef} />
    </div>
  )
}
