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
  board_label: string
  /** `probe_name_hash` as 16-digit hex. */
  probe: string
  /** Human-readable probe name. */
  probe_label: string
  kind: ProbeKind
}

/** One SSE sample (`/probes/stream`). `value` is a string on the wire. */
export interface ProbeView {
  virtual_time_ns: number
  board: string
  /** `probe_name_hash` as 16-digit hex. */
  probe: string
  probe_label: string
  kind: ProbeKind
  value: string
}

export interface ProbeBatch {
  seq: number
  samples: ProbeView[]
}

export interface ActionParam {
  /** `id_hash` of the parameter name as 16-digit hex. */
  name: string
  /** Human-readable parameter name. */
  name_label: string
  unit?: string | null
  description?: string | null
  min?: number | null
  max?: number | null
  default?: number | null
}

export interface ActionRow {
  board_id: string
  board_label?: string
  /** `id_hash` of the device name as 16-digit hex. */
  device: string
  device_label: string
  /** `id_hash` of the action name as 16-digit hex. */
  action: string
  action_label: string
  description: string
  params: ActionParam[]
}

export interface TopologyEndpoint {
  name: string
  label: string
  direction: 'in' | 'out'
  payload: ProbeKind
}

export interface TopologyBoard {
  id: string
  label: string
  kind: string
  endpoints: TopologyEndpoint[]
}

export interface TopologyPortRef {
  path: string
  path_label: string
  port: string
  port_label: string
}

export interface TopologyEdge {
  from: TopologyPortRef
  to: TopologyPortRef
  route: TopologyPortRef[]
  payload: ProbeKind
  uart_ring_len?: number | null
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
