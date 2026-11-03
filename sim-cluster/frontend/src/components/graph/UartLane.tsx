import type { MouseEvent } from 'react'
import type { Series } from '../../hooks/probeStore'
import { fillPreZero, formatUart, PAD_L, PAD_R, UART_H, useCanvasDraw } from './common'
import { formatViewTime } from '../../timeWindow'
import type { LaneHoverHandler } from './hover'
import { hitUartBox, layoutUartBoxes, uartLabelFits } from './uartLayout'

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

    ctx.save()
    ctx.beginPath()
    ctx.rect(x0, 0, x1 - x0, h)
    ctx.clip()
    ctx.font = '11px ui-monospace, monospace'
    ctx.textAlign = 'center'
    ctx.textBaseline = 'middle'
    ctx.lineWidth = 1.5

    const boxes = layoutUartBoxes(series.t, series.v, t0, t1, x0, x1)
    for (const box of boxes) {
      const bw = Math.max(box.x1 - box.x0, 0)
      if (bw <= 0) continue
      ctx.fillStyle = color
      ctx.globalAlpha = 0.16
      ctx.fillRect(box.x0, yTop, bw, yBot - yTop)
      ctx.globalAlpha = 1
      ctx.strokeStyle = color
      ctx.strokeRect(box.x0 + 0.5, yTop + 0.5, Math.max(bw - 1, 0), yBot - yTop - 1)
      const text = formatUart(box.value)
      if (uartLabelFits(bw, text, (s) => ctx.measureText(s).width)) {
        ctx.fillStyle = '#222'
        ctx.fillText(text, box.x0 + bw / 2, h / 2)
      }
    }
    ctx.restore()
  }

  const [hostRef, canvasRef] = useCanvasDraw(draw)

  const report = (ev: MouseEvent<HTMLDivElement>) => {
    if (!onHover) return
    const host = hostRef.current
    if (!host || !series || series.t.length === 0) {
      onHover(null)
      return
    }
    const rect = host.getBoundingClientRect()
    const x = ev.clientX - rect.left
    if (x < PAD_L || x > rect.width - PAD_R) {
      onHover(null)
      return
    }
    const boxes = layoutUartBoxes(series.t, series.v, t0, t1, PAD_L, rect.width - PAD_R)
    const hit = hitUartBox(boxes, x)
    if (!hit) {
      onHover(null)
      return
    }
    onHover({
      x: ev.clientX,
      y: ev.clientY,
      lines: [`t=${formatViewTime(hit.tStart)}`, formatUart(hit.value)],
    })
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
