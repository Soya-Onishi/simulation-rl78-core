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
  /** Local endpoints that connect to `remote`. */
  tips: Tip[]
}

export interface BoardModel {
  id: string
  kind: string
  width: number
  height: number
  left: HandleSpec[]
  right: HandleSpec[]
  /** Endpoints not referenced by any edge. */
  unused: Tip[]
}

export interface EdgeModel {
  id: string
  source: string
  target: string
  sourceHandle: string
  targetHandle: string
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
  return { name: ep.name, direction: ep.direction, payload: ep.payload }
}

/**
 * Builds nodes / edges from the topology. Edges between the same pair of
 * boards are merged into a single React Flow edge and every board gets one
 * handle per remote board.
 */
export function buildModel(topo: Topology): WiringModel {
  const endpointSpec = new Map<string, TopologyEndpoint>()
  for (const b of topo.boards) for (const ep of b.endpoints) endpointSpec.set(`${b.id}/${ep.name}`, ep)

  interface Pair {
    source: string
    target: string
    lines: string[]
  }
  const pairs = new Map<string, Pair>()
  // board -> remote -> endpoints (by name)
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
    const key = [e.from_board, e.to_board].sort().join('\u0000')
    let pair = pairs.get(key)
    if (!pair) pairs.set(key, (pair = { source: e.from_board, target: e.to_board, lines: [] }))
    pair.lines.push(`${e.from_board}.${e.from_endpoint} -> ${e.to_board}.${e.to_endpoint}: ${e.payload}`)
    addLocal(e.from_board, e.to_board, e.from_endpoint, {
      name: e.from_endpoint,
      direction: 'out',
      payload: e.payload,
    })
    addLocal(e.to_board, e.from_board, e.to_endpoint, {
      name: e.to_endpoint,
      direction: 'in',
      payload: e.payload,
    })
  }

  const edges: EdgeModel[] = []
  // board -> handles
  const lefts = new Map<string, HandleSpec[]>()
  const rights = new Map<string, HandleSpec[]>()
  const push = (m: Map<string, HandleSpec[]>, k: string, h: HandleSpec) => {
    const list = m.get(k)
    if (list) list.push(h)
    else m.set(k, [h])
  }
  for (const [key, pair] of pairs) {
    const sh = `s:${pair.target}`
    const th = `t:${pair.source}`
    edges.push({
      id: `e:${key}`,
      source: pair.source,
      target: pair.target,
      sourceHandle: sh,
      targetHandle: th,
      lines: pair.lines,
    })
    const tipsOf = (board: string, remote: string) => [...(local.get(board)?.get(remote)?.values() ?? [])]
    push(rights, pair.source, {
      id: sh,
      type: 'source',
      remote: pair.target,
      tips: tipsOf(pair.source, pair.target),
    })
    push(lefts, pair.target, {
      id: th,
      type: 'target',
      remote: pair.source,
      tips: tipsOf(pair.target, pair.source),
    })
  }

  const boards: BoardModel[] = topo.boards.map((b) => {
    const left = lefts.get(b.id) ?? []
    const right = rights.get(b.id) ?? []
    const u = used.get(b.id)
    const unused = b.endpoints.filter((ep) => !u?.has(ep.name)).map(tipOf)
    return {
      id: b.id,
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
