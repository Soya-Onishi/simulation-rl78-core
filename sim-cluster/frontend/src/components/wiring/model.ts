import type { Topology, TopologyEndpoint } from '../../types'

export interface Tip {
  name: string
  direction: 'in' | 'out'
  payload: string
}

export interface HandleSpec {
  /** React Flow handle id. */
  id: string
  type: 'source' | 'target'
  remote: string
  /** Human-readable label for `remote`. */
  remoteLabel: string
  /** Local endpoints that connect to `remote` (in and out mixed). */
  tips: Tip[]
}

export interface BoardModel {
  /** Public board id (16-digit hex). */
  id: string
  /** Human-readable board label from topology. */
  label: string
  kind: string
  width: number
  height: number
  left: HandleSpec[]
  right: HandleSpec[]
  /** Endpoints not referenced by any edge. */
  unused: Tip[]
}

/**
 * One undirected React Flow edge between a board pair.
 * `a`/`b` are sorted board ids used only as geometry anchors (no payload direction).
 */
export interface EdgeModel {
  id: string
  a: string
  b: string
  aHandle: string
  bHandle: string
  lines: string[]
}

export interface WiringModel {
  boards: BoardModel[]
  edges: EdgeModel[]
}

export const NODE_W = 170
const MIN_H = 64
const PER_HANDLE = 24

function tipOf(ep: TopologyEndpoint): Tip {
  return { name: ep.label, direction: ep.direction, payload: ep.payload }
}

function sortedPair(x: string, y: string): [string, string] {
  return x <= y ? [x, y] : [y, x]
}

/**
 * Builds nodes / edges from the topology. Edges between the same pair of
 * boards are merged into a single undirected React Flow edge. Board order in
 * the edge is lexicographic only so layout has stable anchors; endpoint
 * direction stays on tips for tooltips.
 */
export function buildModel(topo: Topology): WiringModel {
  const endpointSpec = new Map<string, TopologyEndpoint>()
  for (const b of topo.boards) {
    for (const ep of b.endpoints) endpointSpec.set(`${b.id}/${ep.name}`, ep)
  }

  interface Pair {
    a: string
    b: string
    lines: string[]
  }
  const pairs = new Map<string, Pair>()
  // board -> remote -> endpoints (by public endpoint id)
  const local = new Map<string, Map<string, Map<string, Tip>>>()
  const used = new Map<string, Set<string>>()

  const addLocal = (board: string, remote: string, ep: string, fallback: TopologyEndpoint) => {
    let byRemote = local.get(board)
    if (!byRemote) local.set(board, (byRemote = new Map()))
    let eps = byRemote.get(remote)
    if (!eps) byRemote.set(remote, (eps = new Map()))
    eps.set(ep, tipOf(endpointSpec.get(`${board}/${ep}`) ?? fallback))
    let u = used.get(board)
    if (!u) used.set(board, (u = new Set()))
    u.add(ep)
  }

  for (const e of topo.edges) {
    const fromBoard = e.from.path
    const toBoard = e.to.path
    const [a, b] = sortedPair(fromBoard, toBoard)
    const key = `${a}\u0000${b}`
    let pair = pairs.get(key)
    if (!pair) pairs.set(key, (pair = { a, b, lines: [] }))
    pair.lines.push(
      `${e.from.path_label}.${e.from.port_label} -> ${e.to.path_label}.${e.to.port_label}: ${e.payload}`,
    )
    addLocal(fromBoard, toBoard, e.from.port, {
      name: e.from.port,
      label: e.from.port_label,
      direction: 'out',
      payload: e.payload,
    })
    addLocal(toBoard, fromBoard, e.to.port, {
      name: e.to.port,
      label: e.to.port_label,
      direction: 'in',
      payload: e.payload,
    })
  }

  const edges: EdgeModel[] = []
  const lefts = new Map<string, HandleSpec[]>()
  const rights = new Map<string, HandleSpec[]>()
  const push = (m: Map<string, HandleSpec[]>, k: string, h: HandleSpec) => {
    const list = m.get(k)
    if (list) list.push(h)
    else m.set(k, [h])
  }
  const boardLabel = new Map(topo.boards.map((b) => [b.id, b.label]))

  for (const [key, pair] of pairs) {
    // Lexicographic a->b is a geometry anchor only (undirected link).
    const aHandle = `h:${pair.b}`
    const bHandle = `h:${pair.a}`
    edges.push({
      id: `e:${key}`,
      a: pair.a,
      b: pair.b,
      aHandle,
      bHandle,
      lines: pair.lines,
    })
    const tipsOf = (board: string, remote: string) => [
      ...(local.get(board)?.get(remote)?.values() ?? []),
    ]
    push(rights, pair.a, {
      id: aHandle,
      type: 'source',
      remote: pair.b,
      remoteLabel: boardLabel.get(pair.b) ?? pair.b,
      tips: tipsOf(pair.a, pair.b),
    })
    push(lefts, pair.b, {
      id: bHandle,
      type: 'target',
      remote: pair.a,
      remoteLabel: boardLabel.get(pair.a) ?? pair.a,
      tips: tipsOf(pair.b, pair.a),
    })
  }

  const boards: BoardModel[] = topo.boards.map((b) => {
    const left = lefts.get(b.id) ?? []
    const right = rights.get(b.id) ?? []
    const u = used.get(b.id)
    const unused = b.endpoints.filter((ep) => !u?.has(ep.name)).map(tipOf)
    return {
      id: b.id,
      label: b.label,
      kind: b.kind,
      width: NODE_W,
      height: Math.max(MIN_H, PER_HANDLE * Math.max(left.length, right.length) + 24),
      left,
      right,
      unused,
    }
  })
  return { boards, edges }
}
