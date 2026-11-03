import { useState, useSyncExternalStore } from 'react'
import type { ProbeStore } from '../../hooks/probeStore'
import type { ProbeCatalogItem } from '../../types'
import { probeKey } from '../../types'
import AnalogLane from './AnalogLane'
import DigitalLane from './DigitalLane'
import UartLane from './UartLane'
import { colorFor, formatTick, niceStep, PAD_L, PAD_R, useCanvasDraw, WINDOW_STEPS } from './common'

interface Props {
  store: ProbeStore
  catalog: ProbeCatalogItem[]
  watched: Set<string>
  /** Latest virtual time (ns); the right edge of the window. */
  virtualTimeNs: number
}

function Ruler({ t0, t1 }: { t0: number; t1: number }) {
  const draw = (ctx: CanvasRenderingContext2D, w: number, h: number) => {
    const x0 = PAD_L
    const x1 = w - PAD_R
    const span = t1 - t0
    const step = niceStep(span)
    ctx.strokeStyle = '#bbb'
    ctx.fillStyle = '#555'
    ctx.font = '10px sans-serif'
    ctx.textBaseline = 'alphabetic'
    ctx.textAlign = 'center'
    ctx.lineWidth = 1
    ctx.beginPath()
    ctx.moveTo(x0, h - 0.5)
    ctx.lineTo(x1, h - 0.5)
    for (let t = Math.ceil(t0 / step) * step; t <= t1; t += step) {
      const x = Math.round(x0 + ((t - t0) / span) * (x1 - x0)) + 0.5
      ctx.moveTo(x, h - 5)
      ctx.lineTo(x, h)
      ctx.fillText(formatTick(t, step), Math.min(Math.max(x, x0 + 16), x1 - 16), h - 8)
    }
    ctx.stroke()
  }
  const [hostRef, canvasRef] = useCanvasDraw(draw)
  return (
    <div className="lane-plot ruler" ref={hostRef}>
      <canvas ref={canvasRef} />
    </div>
  )
}

export default function GraphView({ store, catalog, watched, virtualTimeNs }: Props) {
  const [stepIdx, setStepIdx] = useState(1)
  const version = useSyncExternalStore(store.subscribe, store.getVersion)

  const lanes = catalog.filter((item) => watched.has(probeKey(item.board, item.probe)))
  const windowNs = WINDOW_STEPS[stepIdx].ns
  // Always follow the newest time as the right edge.
  let t1 = virtualTimeNs
  for (const item of lanes) {
    const s = store.get(probeKey(item.board, item.probe))
    if (s && s.t.length > 0) t1 = Math.max(t1, s.t[s.t.length - 1])
  }
  const t0 = t1 - windowNs

  return (
    <div className="graph">
      <div className="graph-controls">
        <button
          type="button"
          className="text-btn"
          title="拡大（時間幅を狭める）"
          disabled={stepIdx === 0}
          onClick={() => setStepIdx((i) => Math.max(0, i - 1))}
        >
          −
        </button>
        <span className="graph-window">{WINDOW_STEPS[stepIdx].label}</span>
        <button
          type="button"
          className="text-btn"
          title="縮小（時間幅を広げる）"
          disabled={stepIdx === WINDOW_STEPS.length - 1}
          onClick={() => setStepIdx((i) => Math.min(WINDOW_STEPS.length - 1, i + 1))}
        >
          ＋
        </button>
      </div>
      <div className="graph-scroll">
        <div className="lane ruler-row">
          <div className="lane-label" />
          <Ruler t0={t0} t1={t1} />
        </div>
        {lanes.length === 0 && <p className="meta pad">右のプローブでチェックを入れると波形を表示します。</p>}
        {lanes.map((item) => {
          const key = probeKey(item.board, item.probe)
          const series = store.get(key)
          const color = colorFor(key)
          return (
            <div className="lane" key={key}>
              <div className="lane-label" title={`${key} (${item.kind})`} style={{ borderLeftColor: color }}>
                <span className="lane-board">{item.board}</span>
                <span className="lane-probe">{item.probe}</span>
              </div>
              {item.kind === 'analog' ? (
                <AnalogLane series={series} t0={t0} t1={t1} color={color} version={version} />
              ) : item.kind === 'uart' ? (
                <UartLane series={series} t0={t0} t1={t1} color={color} />
              ) : (
                <DigitalLane series={series} t0={t0} t1={t1} color={color} />
              )}
            </div>
          )
        })}
      </div>
    </div>
  )
}
