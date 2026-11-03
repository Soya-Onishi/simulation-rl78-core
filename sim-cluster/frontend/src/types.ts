export type ProbeKind = 'digital' | 'uart' | 'analog'

export interface SessionNode {
  id: string
  kind: string
  state: string
}

export interface StatusEvent {
  id: string
  state: string
  virtual_time_ns: number
  stop_reason?: string | null
  nodes: SessionNode[]
}

export interface LogEvent {
  seq: number
  level: string
  origin: string
  board?: string | null
  text: string
}

export interface ProbeCatalogItem {
  board: string
  probe: string
  kind: ProbeKind
}

/** One SSE sample (`/probes/stream`). `value` is a string on the wire. */
export interface ProbeView {
  virtual_time_ns: number
  board: string
  probe: string
  kind: ProbeKind
  value: string
}

export interface ProbeBatch {
  seq: number
  samples: ProbeView[]
}

export interface ActionParam {
  name: string
  unit?: string | null
  description?: string | null
  min?: number | null
  max?: number | null
  default?: number | null
}

export interface ActionRow {
  board_id: string
  device: string
  action: string
  description: string
  params: ActionParam[]
}

export interface TopologyEndpoint {
  name: string
  direction: 'in' | 'out'
  payload: ProbeKind
}

export interface TopologyBoard {
  id: string
  kind: string
  endpoints: TopologyEndpoint[]
}

export interface TopologyEdge {
  from_board: string
  from_endpoint: string
  to_board: string
  to_endpoint: string
  payload: ProbeKind
}

export interface Topology {
  boards: TopologyBoard[]
  edges: TopologyEdge[]
}

export interface GdbSessionInfo {
  board_id: string
  host: string
  port: number
  user: string
  connected: boolean
}

export type Phase = 'connecting' | 'ready' | 'busy' | 'destroyed'

export function probeKey(board: string, probe: string): string {
  return `${board}/${probe}`
}
