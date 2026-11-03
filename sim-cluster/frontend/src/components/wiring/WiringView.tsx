import {
  Background,
  ReactFlow,
  type Edge,
  type EdgeMouseHandler,
} from '@xyflow/react'
import '@xyflow/react/dist/style.css'
import ELK from 'elkjs/lib/elk.bundled.js'
import { useCallback, useEffect, useMemo, useState, type MouseEvent } from 'react'
import { ApiError, type ViewerClient } from '../../api/client'
import BoardNode, { DirectionIcon, type BoardFlowNode } from './BoardNode'
import GdbDialog, { type GdbDialogState } from './GdbDialog'
import { buildModel, type Tip } from './model'
import type { Topology } from '../../types'

const elk = new ELK()
const nodeTypes = { board: BoardNode }

interface Props {
  client: ViewerClient
  /** Re-fetch the topology when this changes (e.g. the node list). */
  refreshKey: string
  guard: <T>(p: Promise<T>) => Promise<T>
}

type Tooltip =
  | { x: number; y: number; kind: 'tips'; title?: string; tips: Tip[] }
  | { x: number; y: number; kind: 'lines'; lines: string[] }

export default function WiringView({ client, refreshKey, guard }: Props) {
  const [topology, setTopology] = useState<Topology | null>(null)
  const [error, setError] = useState('')
  const [nodes, setNodes] = useState<BoardFlowNode[]>([])
  const [layoutId, setLayoutId] = useState(0)
  const [tooltip, setTooltip] = useState<Tooltip | null>(null)
  const [gdb, setGdb] = useState<GdbDialogState | null>(null)

  const load = useCallback(async () => {
    try {
      let topo: Topology | undefined
      for (let i = 0; i < 25 && !topo; i++) {
        try {
          topo = await guard(client.topology())
        } catch (err) {
          if (err instanceof ApiError && err.status === 503) {
            await new Promise((resolve) => setTimeout(resolve, 200))
            continue
          }
          throw err
        }
      }
      if (!topo) throw new Error('topology is not ready')
      const next = topo
      // Keep the previous object when nothing changed so the layout (and viewport) is not reset.
      setTopology((prev) => (prev && JSON.stringify(prev) === JSON.stringify(next) ? prev : next))
      setError('')
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    }
  }, [client, guard])

  useEffect(() => {
    void load()
  }, [load, refreshKey])

  const model = useMemo(() => (topology ? buildModel(topology) : null), [topology])

  const showTip = useCallback((ev: MouseEvent | null, tips: Tip[] | null, title?: string) => {
    if (!ev || !tips) setTooltip(null)
    else setTooltip({ kind: 'tips', x: ev.clientX, y: ev.clientY, tips, title })
  }, [])

  const attachGdb = useCallback(
    async (boardId: string) => {
      try {
        await guard(client.attachGdb(boardId))
        const info = await client.gdbInfo()
        const ssh = info ? await client.gdbSsh() : ''
        setGdb({ info, ssh })
      } catch (err) {
        if (client.isBusy(err)) return
        const message = err instanceof Error ? err.message : String(err)
        if (err instanceof ApiError && err.status === 409) {
          // Another session already exists: show it, do not replace it.
          try {
            const info = await client.gdbInfo()
            const ssh = info ? await client.gdbSsh() : ''
            setGdb({ info, ssh, error: message })
            return
          } catch {
            // fall through to the plain error
          }
        }
        setGdb({ info: null, ssh: '', error: message })
      }
    },
    [client, guard],
  )

  // Layout with ELK whenever the model changes.
  useEffect(() => {
    if (!model) return
    let cancelled = false
    const graph = {
      id: 'root',
      layoutOptions: {
        'elk.algorithm': 'layered',
        'elk.direction': 'RIGHT',
        'elk.spacing.nodeNode': '40',
        'elk.layered.spacing.nodeNodeBetweenLayers': '90',
      },
      children: model.boards.map((b) => ({ id: b.id, width: b.width, height: b.height })),
      edges: model.edges.map((e) => ({ id: e.id, sources: [e.a], targets: [e.b] })),
    }
    elk
      .layout(graph)
      .then((res) => {
        if (cancelled) return
        const pos = new Map((res.children ?? []).map((c) => [c.id, { x: c.x ?? 0, y: c.y ?? 0 }]))
        setNodes(
          model.boards.map((b) => ({
            id: b.id,
            type: 'board' as const,
            position: pos.get(b.id) ?? { x: 0, y: 0 },
            width: b.width,
            height: b.height,
            data: { board: b, onTip: showTip, onGdb: (id: string) => void attachGdb(id) },
          })),
        )
        setLayoutId((n) => n + 1)
      })
      .catch((err: unknown) => {
        if (!cancelled) setError(`layout failed: ${err instanceof Error ? err.message : String(err)}`)
      })
    return () => {
      cancelled = true
    }
  }, [model, showTip, attachGdb])

  const edges = useMemo<Edge<{ lines: string[] }>[]>(
    () =>
      (model?.edges ?? []).map((e) => ({
        id: e.id,
        source: e.a,
        target: e.b,
        sourceHandle: e.aHandle,
        targetHandle: e.bHandle,
        // Undirected: no arrowhead. Payload direction lives in tooltip lines.
        markerEnd: undefined,
        data: { lines: e.lines },
      })),
    [model],
  )

  const onEdgeMove: EdgeMouseHandler<Edge<{ lines: string[] }>> = (ev, edge) => {
    setTooltip({ kind: 'lines', x: ev.clientX, y: ev.clientY, lines: edge.data?.lines ?? [] })
  }

  return (
    <div className="wiring" onMouseLeave={() => setTooltip(null)}>
      {error && (
        <div className="wiring-error err">
          {error}{' '}
          <button type="button" className="text-btn" onClick={() => void load()}>
            再読み込み
          </button>
        </div>
      )}
      {!error && model && model.boards.length === 0 && <p className="meta pad">ボードがありません。</p>}
      <ReactFlow
        key={layoutId}
        nodes={nodes}
        edges={edges}
        nodeTypes={nodeTypes}
        nodesDraggable={false}
        nodesConnectable={false}
        elementsSelectable
        fitView
        minZoom={0.2}
        proOptions={{ hideAttribution: true }}
        onEdgeMouseEnter={onEdgeMove}
        onEdgeMouseMove={onEdgeMove}
        onEdgeMouseLeave={() => setTooltip(null)}
        onPaneMouseMove={() => tooltip && setTooltip(null)}
      >
        <Background />
      </ReactFlow>
      {tooltip && (
        <div className="tooltip" style={{ left: tooltip.x + 12, top: tooltip.y + 12 }}>
          {tooltip.kind === 'tips' ? (
            <>
              {tooltip.title && <div className="tooltip-title">{tooltip.title}</div>}
              {tooltip.tips.map((t) => (
                <div key={t.name} className="tooltip-row">
                  <DirectionIcon direction={t.direction} />
                  <span>{t.name}</span>
                  <span className="meta">{t.payload}</span>
                </div>
              ))}
            </>
          ) : (
            tooltip.lines.map((line, i) => (
              <div key={i} className="tooltip-row mono">
                {line}
              </div>
            ))
          )}
        </div>
      )}
      {gdb && <GdbDialog state={gdb} onClose={() => setGdb(null)} />}
    </div>
  )
}
