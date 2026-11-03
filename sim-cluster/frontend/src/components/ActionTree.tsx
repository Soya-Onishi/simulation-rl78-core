import { useMemo, useState } from 'react'
import type { ActionParam, ActionRow } from '../types'

interface Props {
  actions: ActionRow[]
  actionsError: string
  running: boolean
  onInvoke: (row: ActionRow, params: Record<string, number>) => Promise<void>
  onReload: () => void
}

function paramHint(p: ActionParam): string {
  const bits: string[] = []
  if (p.unit) bits.push(p.unit)
  if (p.min != null) bits.push(`min ${p.min}`)
  if (p.max != null) bits.push(`max ${p.max}`)
  return bits.join(' · ')
}

function ActionItem({
  row,
  running,
  onInvoke,
}: {
  row: ActionRow
  running: boolean
  onInvoke: Props['onInvoke']
}) {
  const [values, setValues] = useState<Record<string, string>>(() => {
    const init: Record<string, string> = {}
    for (const p of row.params) if (p.default != null) init[p.name] = String(p.default)
    return init
  })
  const [busy, setBusy] = useState(false)
  const [message, setMessage] = useState('')
  const [error, setError] = useState('')

  const run = async () => {
    const params: Record<string, number> = {}
    for (const p of row.params) {
      const raw = (values[p.name] ?? '').trim()
      if (raw === '') {
        if (p.default != null) {
          params[p.name] = Number(p.default)
          continue
        }
        setMessage('')
        setError(`パラメータ ${p.name} を入力してください`)
        return
      }
      const n = Number(raw)
      if (!Number.isFinite(n)) {
        setMessage('')
        setError(`パラメータ ${p.name} が数値ではありません`)
        return
      }
      params[p.name] = n
    }
    setBusy(true)
    setError('')
    setMessage('実行中…')
    try {
      await onInvoke(row, params)
      setMessage('受け付けました')
    } catch (err) {
      setMessage('')
      setError(err instanceof Error ? err.message : '実行に失敗しました')
    } finally {
      setBusy(false)
    }
  }

  return (
    <details className="action-item">
      <summary title={row.description}>{row.action}</summary>
      <div className="action-body">
        {row.description && <div className="meta">{row.description}</div>}
        {row.params.map((p) => {
          const hint = paramHint(p)
          return (
            <label key={p.name} className="action-param">
              <span title={p.description ?? undefined}>{hint ? `${p.name} (${hint})` : p.name}</span>
              <input
                type="number"
                step="any"
                title={p.description ?? undefined}
                min={p.min ?? undefined}
                max={p.max ?? undefined}
                value={values[p.name] ?? ''}
                onChange={(ev) => setValues((prev) => ({ ...prev, [p.name]: ev.target.value }))}
              />
            </label>
          )
        })}
        {running && (
          <button type="button" className="text-btn primary" disabled={busy} onClick={() => void run()}>
            実行
          </button>
        )}
        {message && <div className="ok">{message}</div>}
        {error && <div className="err">{error}</div>}
      </div>
    </details>
  )
}

export default function ActionTree({ actions, actionsError, running, onInvoke, onReload }: Props) {
  const tree = useMemo(() => {
    const boards = new Map<string, Map<string, ActionRow[]>>()
    for (const row of actions) {
      let devices = boards.get(row.board_id)
      if (!devices) boards.set(row.board_id, (devices = new Map()))
      const list = devices.get(row.device)
      if (list) list.push(row)
      else devices.set(row.device, [row])
    }
    return [...boards.entries()].map(([board, devices]) => [board, [...devices.entries()]] as const)
  }, [actions])

  return (
    <div>
      <div className="pane-tools">
        <button type="button" className="text-btn" onClick={onReload}>
          再読み込み
        </button>
      </div>
      {actionsError && <div className="err pad">{actionsError}</div>}
      {tree.length === 0 ? (
        <p className="meta pad">アクションはまだありません。スタート後にボードが公開します。</p>
      ) : (
        <ul className="tree">
          {tree.map(([board, devices]) => (
            <li key={board}>
              <details open>
                <summary>{board}</summary>
                <ul className="tree">
                  {devices.map(([device, rows]) => (
                    <li key={device}>
                      <details open>
                        <summary>{device}</summary>
                        <ul className="tree">
                          {rows.map((row) => (
                            <li key={row.action}>
                              <ActionItem row={row} running={running} onInvoke={onInvoke} />
                            </li>
                          ))}
                        </ul>
                      </details>
                    </li>
                  ))}
                </ul>
              </details>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}
