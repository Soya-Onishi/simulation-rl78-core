import { Handle, Position, type Node, type NodeProps } from '@xyflow/react'
import type { MouseEvent } from 'react'
import type { BoardModel, Tip } from './model'

export type TipHandler = (ev: MouseEvent | null, tips: Tip[] | null, title?: string) => void

export type BoardNodeData = {
  board: BoardModel
  onTip: TipHandler
  onGdb: (boardId: string) => void
}

export type BoardFlowNode = Node<BoardNodeData, 'board'>

export function DirectionIcon({ direction }: { direction: 'in' | 'out' }) {
  return (
    <svg
      className={`dir-icon ${direction}`}
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2.5"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <title>{direction === 'in' ? '入力' : '出力'}</title>
      {direction === 'in' ? (
        <>
          <path d="M3 12h14" />
          <path d="M12 6l6 6-6 6" />
          <path d="M21 4v16" />
        </>
      ) : (
        <>
          <path d="M3 4v16" />
          <path d="M7 12h14" />
          <path d="M16 6l6 6-6 6" />
        </>
      )}
    </svg>
  )
}

function KindIcon({ kind }: { kind: string }) {
  const common = {
    width: 22,
    height: 22,
    viewBox: '0 0 24 24',
    fill: 'none',
    stroke: 'currentColor',
    strokeWidth: 1.6,
    strokeLinecap: 'round' as const,
    strokeLinejoin: 'round' as const,
  }
  if (kind === 'rl78') {
    // chip
    return (
      <svg {...common}>
        <title>rl78</title>
        <rect x="6" y="6" width="12" height="12" rx="1" />
        <path d="M9 3v3M12 3v3M15 3v3M9 18v3M12 18v3M15 18v3M3 9h3M3 12h3M3 15h3M18 9h3M18 12h3M18 15h3" />
      </svg>
    )
  }
  if (kind === 'plant') {
    // lag (first-order lag step response)
    return (
      <svg {...common}>
        <title>plant</title>
        <path d="M3 20h18M3 4v16" />
        <path d="M3 18C8 18 9 6 21 6" />
      </svg>
    )
  }
  return (
    <svg {...common}>
      <title>{kind}</title>
      <rect x="5" y="5" width="14" height="14" rx="2" />
    </svg>
  )
}

export default function BoardNode({ data }: NodeProps<BoardFlowNode>) {
  const { board, onTip, onGdb } = data
  const handles = (specs: typeof board.left, position: Position) =>
    specs.map((h, i) => (
      <Handle
        key={h.id}
        id={h.id}
        type={h.type}
        position={position}
        style={{ top: `${((i + 1) / (specs.length + 1)) * 100}%` }}
        onMouseEnter={(ev) => onTip(ev, h.tips, `${board.id} ↔ ${h.remote}`)}
        onMouseMove={(ev) => onTip(ev, h.tips, `${board.id} ↔ ${h.remote}`)}
        onMouseLeave={() => onTip(null, null)}
      />
    ))

  return (
    <div className="board-node" style={{ width: board.width, height: board.height }}>
      {handles(board.left, Position.Left)}
      {handles(board.right, Position.Right)}
      <div className="board-node-head">
        <KindIcon kind={board.kind} />
        <span className="board-node-id" title={`${board.id} (${board.kind})`}>
          {board.id}
        </span>
      </div>
      {board.kind === 'rl78' && (
        <button type="button" className="text-btn nodrag nopan gdb-btn" onClick={() => onGdb(board.id)}>
          GDB
        </button>
      )}
      {board.unused.length > 0 && (
        <div
          className="unused-port nodrag"
          onMouseEnter={(ev) => onTip(ev, board.unused, `${board.id} (未接続)`)}
          onMouseMove={(ev) => onTip(ev, board.unused, `${board.id} (未接続)`)}
          onMouseLeave={() => onTip(null, null)}
        />
      )}
    </div>
  )
}
