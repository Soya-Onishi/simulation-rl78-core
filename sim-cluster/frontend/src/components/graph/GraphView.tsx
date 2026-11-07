import { useEffect, useRef, useState, useSyncExternalStore, type MouseEvent as ReactMouseEvent } from 'react'
import type { ProbeStore } from '../../hooks/probeStore'
import type { ProbeCatalogItem } from '../../types'
import { probeKey } from '../../types'
import {
  advanceLiveEnd,
  clampViewEnd,
  formatViewTime,
  RETAIN_NS,
  DEFAULT_TICK_STEP_INDEX,
  TICK_STEPS,
  tickStepIndexForSpan,
  viewExpired,
  windowNsForTick,
} from '../../timeWindow'
import AnalogLane from './AnalogLane'
import DigitalLane from './DigitalLane'
import UartLane from './UartLane'
import { colorFor, fillPreZero, formatTick, PAD_L, PAD_R, useCanvasDraw } from './common'
import type { LaneHoverInfo } from './hover'

interface Props {
  store: ProbeStore
  catalog: ProbeCatalogItem[]
  watched: Set<string>
  /** Latest virtual time (ns); drives live follow. */
  virtualTimeNs: number
}

/** Ignore clicks shorter than this (px) so lane hover still works. */
const SELECT_MIN_PX = 8

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

function plotGeom(scroll: HTMLElement): { left: number; width: number; plot: DOMRect } | null {
  const plot = scroll.querySelector('.lane-plot') as HTMLElement | null
  if (!plot) return null
  const scrollRect = scroll.getBoundingClientRect()
  const plotRect = plot.getBoundingClientRect()
  return {
    left: plotRect.left - scrollRect.left + scroll.scrollLeft,
    width: plotRect.width,
    plot: plotRect,
  }
}

function timeAtClientX(clientX: number, plot: DOMRect, t0: number, t1: number): number | null {
  if (t1 <= t0) return null
  const x = clientX - plot.left
  const x0 = PAD_L
  const x1 = plot.width - PAD_R
  if (x1 <= x0) return null
  const clamped = Math.min(Math.max(x, x0), x1)
  return t0 + ((clamped - x0) / (x1 - x0)) * (t1 - t0)
}

export default function GraphView({ store, catalog, watched, virtualTimeNs }: Props) {
  const [stepIdx, setStepIdx] = useState(
    DEFAULT_TICK_STEP_INDEX >= 0 ? DEFAULT_TICK_STEP_INDEX : 0,
  )
  const [followLive, setFollowLive] = useState(true)
  const [viewEndNs, setViewEndNs] = useState(0)
  const [hover, setHover] = useState<LaneHoverInfo | null>(null)
  const [selectPx, setSelectPx] = useState<{ left: number; width: number } | null>(null)
  const [dragging, setDragging] = useState(false)
  const scrollRef = useRef<HTMLDivElement>(null)
  const dragRef = useRef<{
    originClientX: number
    originTime: number
    plotLeftInScroll: number
  } | null>(null)
  const viewRef = useRef({ t0: 0, t1: 0, liveEnd: 0 })
  const liveEndRef = useRef(0)
  const virtualTimeRef = useRef(virtualTimeNs)
  virtualTimeRef.current = virtualTimeNs
  const applySelectionRef = useRef<(a: number, b: number) => void>(() => {})
  const prevVirtualTimeRef = useRef(virtualTimeNs)
  const version = useSyncExternalStore(store.subscribe, store.getVersion)

  const tickNs = TICK_STEPS[stepIdx].tickNs
  const windowNs = windowNsForTick(tickNs)

  // Reset rewinds virtual time; snap the live edge and return to follow-live.
  useEffect(() => {
    if (virtualTimeNs < prevVirtualTimeRef.current) {
      liveEndRef.current = 0
      setFollowLive(true)
      setViewEndNs(0)
    }
    prevVirtualTimeRef.current = virtualTimeNs
  }, [virtualTimeNs])

  const liveEnd = advanceLiveEnd(liveEndRef.current, virtualTimeNs, tickNs)
  liveEndRef.current = liveEnd

  useEffect(() => {
    if (followLive) setViewEndNs(liveEnd)
  }, [followLive, liveEnd])

  const expired = !followLive && viewExpired(viewEndNs, virtualTimeNs)
  useEffect(() => {
    if (expired) setHover(null)
  }, [expired])
  const t1 = followLive ? liveEnd : viewEndNs
  const t0 = t1 - windowNs
  viewRef.current = { t0, t1, liveEnd }

  const lanes = catalog.filter((item) => watched.has(probeKey(item.board, item.probe)))

  const jump = (nextEnd: number) => {
    setFollowLive(false)
    setViewEndNs(clampViewEnd(nextEnd, liveEnd, tickNs))
  }

  const onScale = (nextIdx: number) => {
    const nextTick = TICK_STEPS[nextIdx].tickNs
    const nextLive = advanceLiveEnd(0, virtualTimeRef.current, nextTick)
    liveEndRef.current = nextLive
    setStepIdx(nextIdx)
    if (followLive) return
    setViewEndNs(clampViewEnd(viewEndNs, nextLive, nextTick))
  }

  const applySelection = (timeA: number, timeB: number) => {
    const left = Math.min(timeA, timeB)
    const right = Math.max(timeA, timeB)
    const span = right - left
    if (!(span > 0)) return
    const idx = tickStepIndexForSpan(span)
    const nextTick = TICK_STEPS[idx].tickNs
    // Always derive live end from the latest virtual time (not a closed-over prop).
    const nextLive = advanceLiveEnd(0, virtualTimeRef.current, nextTick)
    liveEndRef.current = nextLive
    setStepIdx(idx)
    setFollowLive(false)
    setViewEndNs(clampViewEnd(right, nextLive, nextTick))
  }
  applySelectionRef.current = applySelection

  const onScrollMouseDown = (ev: ReactMouseEvent<HTMLDivElement>) => {
    if (ev.button !== 0) return
    const scroll = scrollRef.current
    if (!scroll || expired) return
    const target = ev.target as HTMLElement
    if (target.closest('.lane-label')) return
    const geom = plotGeom(scroll)
    if (!geom) return
    const { t0: vt0, t1: vt1 } = viewRef.current
    const time = timeAtClientX(ev.clientX, geom.plot, vt0, vt1)
    if (time === null) return
    dragRef.current = {
      originClientX: ev.clientX,
      originTime: time,
      plotLeftInScroll: geom.left,
    }
    setDragging(true)
    setHover(null)
    setSelectPx({ left: ev.clientX - geom.plot.left + geom.left, width: 0 })
    ev.preventDefault()
  }

  useEffect(() => {
    const onMove = (ev: MouseEvent) => {
      const drag = dragRef.current
      const scroll = scrollRef.current
      if (!drag || !scroll) return
      const geom = plotGeom(scroll)
      if (!geom) return
      const a = drag.originClientX - geom.plot.left + geom.left
      const b = ev.clientX - geom.plot.left + geom.left
      const left = Math.min(a, b)
      const right = Math.max(a, b)
      setSelectPx({ left, width: Math.max(0, right - left) })
      setHover(null)
    }
    const onUp = (ev: MouseEvent) => {
      const drag = dragRef.current
      if (!drag) return
      dragRef.current = null
      setDragging(false)
      setSelectPx(null)
      const dx = Math.abs(ev.clientX - drag.originClientX)
      if (dx < SELECT_MIN_PX) return
      const scroll = scrollRef.current
      if (!scroll) return
      const geom = plotGeom(scroll)
      if (!geom) return
      const { t0: vt0, t1: vt1 } = viewRef.current
      const endTime = timeAtClientX(ev.clientX, geom.plot, vt0, vt1)
      if (endTime === null) return
      // Ref so this []-deps listener always runs the latest applySelection.
      applySelectionRef.current(drag.originTime, endTime)
    }
    window.addEventListener('mousemove', onMove)
    window.addEventListener('mouseup', onUp)
    return () => {
      window.removeEventListener('mousemove', onMove)
      window.removeEventListener('mouseup', onUp)
    }
  }, [])

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
          {'<'}
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
          {'>'}
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
          {'>>'}
        </button>
      </div>
      {expired ? (
        <p className="meta pad graph-expired">
          {
            '表示中の区間は保持期間（過去 10 時間）を超えました。>> で最新に戻るか、タイムスケールを変更してください。'
          }
        </p>
      ) : (
        <div
          className="graph-scroll"
          ref={scrollRef}
          onMouseDown={onScrollMouseDown}
          onMouseLeave={() => {
            if (!dragRef.current) setHover(null)
          }}
        >
          {selectPx && selectPx.width > 0 && (
            <div
              className="graph-select-rect"
              style={{ left: selectPx.left, width: selectPx.width }}
            />
          )}
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
                  <span className="lane-board">{item.board_label || item.board}</span>
                  <span className="lane-probe">{item.probe}</span>
                </div>
                {item.kind === 'analog' ? (
                  <div className="lane-plot-wrap">
                    <PreZeroOverlay t0={t0} t1={t1} />
                    <AnalogLane
                      series={series}
                      t0={t0}
                      t1={t1}
                      color={color}
                      version={version}
                      onHover={dragging ? undefined : setHover}
                    />
                  </div>
                ) : item.kind === 'uart' ? (
                  <UartLane
                    series={series}
                    t0={t0}
                    t1={t1}
                    color={color}
                    onHover={dragging ? undefined : setHover}
                  />
                ) : (
                  <DigitalLane
                    series={series}
                    t0={t0}
                    t1={t1}
                    color={color}
                    onHover={dragging ? undefined : setHover}
                  />
                )}
              </div>
            )
          })}
        </div>
      )}
      {hover && !selectPx && (
        <div className="graph-hover-dialog" style={{ left: hover.x + 12, top: hover.y + 12 }}>
          {hover.lines.map((line, i) => (
            <div key={i} className="mono">
              {line}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}
