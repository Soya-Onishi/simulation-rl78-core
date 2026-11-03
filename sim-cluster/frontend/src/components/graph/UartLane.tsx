import type { MouseEvent } from 'react'
import type { Series } from '../../hooks/probeStore'
import { lastAtOrBefore } from '../../hooks/probeStore'
import { fillPreZero, formatUart, PAD_L, PAD_R, UART_H, useCanvasDraw } from './common'
import {
  hoverUart,
  timeFromClientX,
  UART_FRAME_NS,
  type LaneHoverHandler,
} from './hover'

interface Props {
  series: Series | undefined
  t0: number
  t1: number
  color: string
  onHover?: LaneHoverHandler
}

export default function UartLane({ series, t0, t1, color, onHover }: Props) {
  const draw = (ctx: CanvasRenderingContext2D, w: number, h: number) => {
    const x0 = PAD_L
    const x1 = w - PAD_R
    const yTop = 6
    const yBot = h - 6

    fillPreZero(ctx, t0, t1, x0, x1, h)

    ctx.strokeStyle = '#ddd'
    ctx.lineWidth = 1
    ctx.beginPath()
    ctx.moveTo(x0, h / 2 + 0.5)
    ctx.lineTo(x1, h / 2 + 0.5)
    ctx.stroke()

    if (!series || series.t.length === 0) return
    const { t, v } = series
    const span = t1 - t0
    const xOf = (time: number) => x0 + ((time - t0) / span) * (x1 - x0)

    ctx.save()
    ctx.beginPath()
    ctx.rect(x0, 0, x1 - x0, h)
    ctx.clip()
    ctx.font = '11px ui-monospace, monospace'
    ctx.textAlign = 'center'
    ctx.textBaseline = 'middle'
    ctx.lineWidth = 1.2

    let i = lastAtOrBefore(t, t0)
    if (i < 0) i = 0
    for (let j = i; j < t.length; j++) {
      const start = t[j]
      if (start > t1) break
      const next = j + 1 < t.length ? t[j + 1] : Infinity
      const end = Math.min(next, start + UART_FRAME_NS)
      const xa = xOf(start)
      const xb = xOf(end)
      if (xb < x0) continue
      const bw = Math.max(xb - xa, 1)
      ctx.fillStyle = color
      ctx.globalAlpha = 0.12
      ctx.fillRect(xa, yTop, bw, yBot - yTop)
      ctx.globalAlpha = 1
      ctx.strokeStyle = color
      ctx.strokeRect(xa + 0.5, yTop + 0.5, Math.max(bw - 1, 0), yBot - yTop - 1)
      const text = formatUart(v[j])
      if (ctx.measureText(text).width + 6 <= bw) {
        ctx.fillStyle = '#222'
        ctx.fillText(text, xa + bw / 2, h / 2)
      }
    }
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
    const lines = hoverUart(series, time)
    onHover(lines ? { x: ev.clientX, y: ev.clientY, lines } : null)
  }

  return (
    <div
      className="lane-plot"
      style={{ height: UART_H }}
      ref={hostRef}
      onMouseMove={report}
      onMouseLeave={() => onHover?.(null)}
    >
      <canvas ref={canvasRef} />
    </div>
  )
}
