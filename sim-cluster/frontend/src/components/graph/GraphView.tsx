import { useEffect, useRef, useState, useSyncExternalStore } from 'react'
import type { ProbeStore } from '../../hooks/probeStore'
import type { ProbeCatalogItem } from '../../types'
import { probeKey } from '../../types'
import {
  advanceLiveEnd,
  clampViewEnd,
  formatViewTime,
  RETAIN_NS,
  TICK_STEPS,
  viewExpired,
  windowNsForTick,
} from '../../timeWindow'
import AnalogLane from './AnalogLane'
import DigitalLane from './DigitalLane'
import UartLane from './UartLane'
import { colorFor, fillPreZero, formatTick, PAD_L, PAD_R, useCanvasDraw } from './common'

interface Props {
  store: ProbeStore
  catalog: ProbeCatalogItem[]
  watched: Set<string>
  /** Latest virtual time (ns); drives live follow. */
  virtualTimeNs: number
}

function Ruler({ t0, t1, tickNs }: { t0: number; t1: number; tickNs: number }) {
  const draw = (ctx: CanvasRenderingContext2D, w: number, h: number) => {
    const x0 = PAD_L
    const x1 = w - PAD_R
    const span = t1 - t0
    fillPreZero(ctx, t0, t1, x0, x1, h)
    ctx.strokeStyle = '#bbb'
    ctx.fillStyle = '#555'
    ctx.font = '10px sans-serif'
    ctx.textBaseline = 'alphabetic'
    ctx.textAlign = 'center'
    ctx.lineWidth = 1
    ctx.beginPath()
    ctx.moveTo(x0, h - 0.5)
    ctx.lineTo(x1, h - 0.5)
    if (span > 0 && tickNs > 0) {
      const first = Math.ceil(t0 / tickNs) * tickNs
      for (let t = first; t <= t1 + tickNs * 1e-9; t += tickNs) {
        const x = Math.round(x0 + ((t - t0) / span) * (x1 - x0)) + 0.5
        ctx.moveTo(x, h - 5)
        ctx.lineTo(x, h)
        ctx.fillText(formatTick(t, tickNs), Math.min(Math.max(x, x0 + 16), x1 - 16), h - 8)
      }
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

function PreZeroOverlay({ t0, t1 }: { t0: number; t1: number }) {
  if (t0 >= 0 || t1 <= t0) return null
  const span = t1 - t0
  const frac = Math.min(1, Math.max(0, (0 - t0) / span))
  const left = PAD_L
  const rightPad = PAD_R
  return (
    <div
      className="pre-zero-overlay"
      style={{
        left,
        width: `calc((100% - ${left + rightPad}px) * ${frac})`,
      }}
    />
  )
}

export default function GraphView({ store, catalog, watched, virtualTimeNs }: Props) {
  const [stepIdx, setStepIdx] = useState(1)
  const [followLive, setFollowLive] = useState(true)
  const [viewEndNs, setViewEndNs] = useState(0)
  const liveEndRef = useRef(0)
  const version = useSyncExternalStore(store.subscribe, store.getVersion)

  const tickNs = TICK_STEPS[stepIdx].tickNs
  const windowNs = windowNsForTick(tickNs)

  const liveEnd = advanceLiveEnd(liveEndRef.current, virtualTimeNs, tickNs)
  liveEndRef.current = liveEnd

  useEffect(() => {
    if (followLive) setViewEndNs(liveEnd)
  }, [followLive, liveEnd])

  const expired = !followLive && viewExpired(viewEndNs, virtualTimeNs)
  const t1 = followLive ? liveEnd : viewEndNs
  const t0 = t1 - windowNs

  const lanes = catalog.filter((item) => watched.has(probeKey(item.board, item.probe)))

  const jump = (nextEnd: number) => {
    setFollowLive(false)
    setViewEndNs(clampViewEnd(nextEnd, liveEnd, tickNs))
  }

  const onScale = (nextIdx: number) => {
    const nextTick = TICK_STEPS[nextIdx].tickNs
    setStepIdx(nextIdx)
    if (followLive) return
    // Keep the current right edge so the viewed slice becomes the newest end.
    setViewEndNs(clampViewEnd(viewEndNs, liveEndRef.current, nextTick))
  }

  const sliderMin = Math.max(0, liveEnd - RETAIN_NS + windowNs)
  const sliderMax = Math.max(sliderMin, liveEnd)
  const tickMin = Math.floor(sliderMin / tickNs)
  const tickMax = Math.floor(sliderMax / tickNs)
  const tickValue = Math.min(tickMax, Math.max(tickMin, Math.floor(t1 / tickNs)))

  return (
    <div className="graph">
      <div className="graph-controls">
        <button
          type="button"
          className="text-btn"
          title="拡大（時間幅を狭める）"
          disabled={stepIdx === 0}
          onClick={() => onScale(Math.max(0, stepIdx - 1))}
        >
          −
        </button>
        <span className="graph-window">{TICK_STEPS[stepIdx].label}</span>
        <button
          type="button"
          className="text-btn"
          title="縮小（時間幅を広げる）"
          disabled={stepIdx === TICK_STEPS.length - 1}
          onClick={() => onScale(Math.min(TICK_STEPS.length - 1, stepIdx + 1))}
        >
          ＋
        </button>
        <span className="graph-spacer" />
        <input
          className="graph-slider"
          type="range"
          min={tickMin}
          max={tickMax}
          step={1}
          value={tickValue}
          disabled={expired || tickMax <= tickMin}
          onChange={(ev) => jump(Number(ev.target.value) * tickNs)}
          title="表示位置"
        />
        <button
          type="button"
          className="text-btn"
          title="1目盛り戻る"
          disabled={expired || t1 <= sliderMin}
          onClick={() => jump(t1 - tickNs)}
        >
          [&lt;]
        </button>
        <span className="graph-range mono">
          {formatViewTime(t0)} - {formatViewTime(t1)}/
        </span>
        <button
          type="button"
          className="text-btn"
          title="1目盛り進む"
          disabled={expired || followLive || t1 >= liveEnd}
          onClick={() => jump(t1 + tickNs)}
        >
          [&gt;]
        </button>
        <button
          type="button"
          className="text-btn"
          title="最新へ"
          disabled={followLive}
          onClick={() => {
            setFollowLive(true)
            setViewEndNs(liveEndRef.current)
          }}
        >
          [&gt;&gt;]
        </button>
      </div>
      {expired ? (
        <p className="meta pad graph-expired">
          {
            '表示中の区間は保持期間（過去 10 時間）を超えました。[>>] で最新に戻るか、タイムスケールを変更してください。'
          }
        </p>
      ) : (
        <div className="graph-scroll">
          <div className="lane ruler-row">
            <div className="lane-label" />
            <Ruler t0={t0} t1={t1} tickNs={tickNs} />
          </div>
          {lanes.length === 0 && (
            <p className="meta pad">右のプローブでチェックを入れると波形を表示します。</p>
          )}
          {lanes.map((item) => {
            const key = probeKey(item.board, item.probe)
            const series = store.get(key)
            const color = colorFor(key)
            return (
              <div className="lane" key={key}>
                <div
                  className="lane-label"
                  title={`${key} (${item.kind})`}
                  style={{ borderLeftColor: color }}
                >
                  <span className="lane-board">{item.board}</span>
                  <span className="lane-probe">{item.probe}</span>
                </div>
                {item.kind === 'analog' ? (
                  <div className="lane-plot-wrap">
                    <PreZeroOverlay t0={t0} t1={t1} />
                    <AnalogLane series={series} t0={t0} t1={t1} color={color} version={version} />
                  </div>
                ) : item.kind === 'uart' ? (
                  <UartLane series={series} t0={t0} t1={t1} color={color} />
                ) : (
                  <DigitalLane series={series} t0={t0} t1={t1} color={color} />
                )}
              </div>
            )
          })}
        </div>
      )}
    </div>
  )
}
