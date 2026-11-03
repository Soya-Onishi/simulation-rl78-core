import { useEffect, useRef } from 'react'
import type { LogEvent } from '../types'

const LEVEL_CLASS: Record<string, string> = {
  info: 'lv-info',
  warn: 'lv-warn',
  error: 'lv-error',
}

export default function LogPane({ logs }: { logs: LogEvent[] }) {
  const ref = useRef<HTMLPreElement>(null)
  const stick = useRef(true)

  useEffect(() => {
    const el = ref.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [logs])

  return (
    <pre
      className="logpane"
      ref={ref}
      onScroll={(ev) => {
        const el = ev.currentTarget
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24
      }}
    >
      {logs.map((log) => {
        const level = log.level || 'info'
        const who = log.board ? `node ${log.board}` : log.origin || 'log'
        return (
          <div key={log.seq}>
            <span className={LEVEL_CLASS[level] ?? 'lv-info'}>[{level}]</span> {who}: {log.text}
          </div>
        )
      })}
    </pre>
  )
}
