import type { ReactNode } from 'react'
import type { StatusEvent } from '../types'

export function formatTime(ns: number | null | undefined): string {
  if (ns == null) return '—'
  if (ns < 1e3) return `${ns} ns`
  if (ns < 1e6) return `${(ns / 1e3).toFixed(3)} µs`
  if (ns < 1e9) return `${(ns / 1e6).toFixed(3)} ms`
  return `${(ns / 1e9).toFixed(6)} s`
}

function Icon({ children }: { children: ReactNode }) {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      {children}
    </svg>
  )
}

const PlayIcon = () => (
  <Icon>
    <path d="M7 4.5v15l13-7.5z" />
  </Icon>
)
const StopIcon = () => (
  <Icon>
    <rect x="6" y="6" width="12" height="12" rx="1" />
  </Icon>
)
const ResetIcon = () => (
  <Icon>
    <path d="M12 5a7 7 0 1 1-6.7 9h2.1A5 5 0 1 0 12 7v3L7 6l5-4z" />
  </Icon>
)
const DestroyIcon = () => (
  <Icon>
    <path d="M9 3h6l1 2h4v2H4V5h4zM6 9h12l-1 12H7z" />
  </Icon>
)

interface Props {
  simId: string
  status: StatusEvent | null
  statusError: string
  commandError: string
  onCommand: (name: 'start' | 'stop' | 'reset') => void
  onDestroy: () => void
  onCsv: () => void
}

export default function TopBar({
  simId,
  status,
  statusError,
  commandError,
  onCommand,
  onDestroy,
  onCsv,
}: Props) {
  const state = status?.state ?? ''
  const dead = state === 'exited'
  const running = state === 'running'
  const error = commandError || statusError

  return (
    <header className="topbar">
      <strong className="topbar-title" title={simId}>
        {simId}
      </strong>
      <span className="topbar-state">{state}</span>
      <div className="topbar-buttons">
        <button
          type="button"
          className="icon-btn start"
          title="スタート"
          aria-label="スタート"
          disabled={!status || dead || running}
          onClick={() => onCommand('start')}
        >
          <PlayIcon />
        </button>
        <button
          type="button"
          className="icon-btn stop"
          title="ストップ"
          aria-label="ストップ"
          disabled={!status || dead || !running}
          onClick={() => onCommand('stop')}
        >
          <StopIcon />
        </button>
        <button
          type="button"
          className="icon-btn"
          title="リセット"
          aria-label="リセット"
          disabled={!status || dead}
          onClick={() => onCommand('reset')}
        >
          <ResetIcon />
        </button>
        <button
          type="button"
          className="icon-btn danger"
          title="破棄"
          aria-label="破棄"
          onClick={onDestroy}
        >
          <DestroyIcon />
        </button>
      </div>
      <span className="topbar-error" title={error}>
        {error}
      </span>
      <button type="button" className="text-btn" onClick={onCsv}>
        CSV
      </button>
      <span className="topbar-time" title={`${status?.virtual_time_ns ?? 0} ns`}>
        <span className="meta">仮想時間</span> <strong>{formatTime(status?.virtual_time_ns)}</strong>
      </span>
    </header>
  )
}
