import { useMemo } from 'react'
import type { ProbeCatalogItem } from '../types'
import { probeKey } from '../types'

interface Props {
  catalog: ProbeCatalogItem[]
  watched: Set<string>
  onChange: (next: Set<string>) => void
}

export default function ProbeTree({ catalog, watched, onChange }: Props) {
  const boards = useMemo(() => {
    const map = new Map<string, ProbeCatalogItem[]>()
    for (const item of catalog) {
      const list = map.get(item.board)
      if (list) list.push(item)
      else map.set(item.board, [item])
    }
    return [...map.entries()]
  }, [catalog])

  if (boards.length === 0) return <p className="meta pad">カタログはまだありません。</p>

  const toggle = (key: string, checked: boolean) => {
    const next = new Set(watched)
    if (checked) next.add(key)
    else next.delete(key)
    onChange(next)
  }

  return (
    <ul className="tree">
      {boards.map(([board, items]) => (
        <li key={board}>
          <details open>
            <summary>{board}</summary>
            <ul className="tree">
              {items.map((item) => {
                const key = probeKey(item.board, item.probe)
                return (
                  <li key={key}>
                    <label className="tree-probe">
                      <input
                        type="checkbox"
                        checked={watched.has(key)}
                        onChange={(ev) => toggle(key, ev.target.checked)}
                      />
                      <span>{item.probe}</span>
                      <span className="meta">{item.kind}</span>
                    </label>
                  </li>
                )
              })}
            </ul>
          </details>
        </li>
      ))}
    </ul>
  )
}
