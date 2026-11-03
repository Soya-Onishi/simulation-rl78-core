import type {
  ActionRow,
  GdbSessionInfo,
  ProbeCatalogItem,
  ProbeKind,
  Topology,
} from '../types'

export class ApiError extends Error {
  status: number
  code: string
  sentToken: string | null

  constructor(status: number, code: string, message: string, sentToken: string | null) {
    super(message)
    this.status = status
    this.code = code
    this.sentToken = sentToken
  }
}

interface RequestOptions {
  body?: unknown
  keepalive?: boolean
}

function errorMessage(body: unknown, fallback: string): { code: string; message: string } {
  if (body && typeof body === 'object' && 'error' in body) {
    const err = (body as { error: unknown }).error
    if (typeof err === 'string') return { code: '', message: err }
    if (err && typeof err === 'object') {
      const e = err as { code?: string; message?: string }
      return { code: e.code ?? '', message: e.message ?? fallback }
    }
  }
  return { code: '', message: fallback }
}

/** Fetch / SSE helper bound to one simulation id and (after the first viewer event) a viewer token. */
export class ViewerClient {
  readonly simId: string
  token: string | null = null

  constructor(simId: string) {
    this.simId = simId
  }

  get base(): string {
    return `/api/simulation/${encodeURIComponent(this.simId)}`
  }

  headers(extra: Record<string, string> = {}): Record<string, string> {
    const headers = { ...extra }
    if (this.token) headers['x-viewer-token'] = this.token
    return headers
  }

  withViewer(url: string): string {
    if (!this.token) return url
    return `${url}${url.includes('?') ? '&' : '?'}viewer=${encodeURIComponent(this.token)}`
  }

  /** True when `err` is a viewer_busy 409 raised for the token that is currently in use. */
  isBusy(err: unknown): boolean {
    if (!(err instanceof ApiError)) return false
    if (err.status !== 409 || err.code !== 'viewer_busy') return false
    if (!this.token) return true
    return err.sentToken === this.token
  }

  async raw(method: string, path: string, opts: RequestOptions = {}): Promise<Response> {
    const sentToken = this.token
    const headers = this.headers(opts.body !== undefined ? { 'content-type': 'application/json' } : {})
    const res = await fetch(`${this.base}${path}`, {
      method,
      headers,
      body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined,
      keepalive: opts.keepalive,
    })
    if (!res.ok) {
      const body: unknown = await res.json().catch(() => ({}))
      const { code, message } = errorMessage(body, res.statusText || `HTTP ${res.status}`)
      throw new ApiError(res.status, code, message, sentToken)
    }
    return res
  }

  async json<T>(method: string, path: string, opts: RequestOptions = {}): Promise<T> {
    const res = await this.raw(method, path, opts)
    return (await res.json()) as T
  }

  // --- control -----------------------------------------------------------

  command(name: 'start' | 'stop' | 'reset'): Promise<Response> {
    return this.raw('POST', `/${name}`)
  }

  async destroy(): Promise<void> {
    const res = await fetch(this.base, { method: 'DELETE', headers: this.headers() })
    if (!res.ok) {
      const body: unknown = await res.json().catch(() => ({}))
      const { code, message } = errorMessage(body, res.statusText)
      throw new ApiError(res.status, code, message, this.token)
    }
  }

  // --- probes ------------------------------------------------------------

  async probeCatalog(): Promise<ProbeCatalogItem[]> {
    // The catalog is 503 until the nodes have advertised their probes.
    for (let i = 0; i < 50; i++) {
      try {
        return (await this.json<ProbeCatalogItem[] | null>('GET', '/probes/catalog')) ?? []
      } catch (err) {
        if (err instanceof ApiError && err.status === 503) {
          await new Promise((resolve) => setTimeout(resolve, 200))
          continue
        }
        throw err
      }
    }
    throw new Error('probe catalog is not ready')
  }

  async putWatch(probes: { board: string; name: string }[]): Promise<void> {
    await this.raw('PUT', '/probes/watch', { body: { probes } })
  }

  async clearWatch(keepalive = false): Promise<void> {
    await this.raw('DELETE', '/probes/watch', { keepalive })
  }

  /** Download probes.csv like the old UI (waits for csv-status before saving). */
  async downloadCsv(): Promise<void> {
    let res: Response
    try {
      res = await this.raw('GET', '/probes.csv')
    } catch (err) {
      if (err instanceof ApiError && err.status === 504) {
        throw new Error('CSV の取得がタイムアウトしました')
      }
      throw err
    }
    const requestId = res.headers.get('x-probe-csv-id')
    const blob = await res.blob()
    if (requestId) {
      const status = await this.json<{ failed?: boolean }>(
        'GET',
        `/probes/csv-status?request_id=${encodeURIComponent(requestId)}`,
      )
      if (status.failed) throw new Error('CSV の取得が完了しませんでした')
    }
    const link = document.createElement('a')
    link.href = URL.createObjectURL(blob)
    link.download = 'probes.csv'
    link.click()
    URL.revokeObjectURL(link.href)
  }

  // --- actions -----------------------------------------------------------

  async actions(): Promise<ActionRow[]> {
    const body = await this.json<{ actions?: ActionRow[] }>('GET', '/actions')
    return body.actions ?? []
  }

  invokeAction(
    row: Pick<ActionRow, 'board_id' | 'device' | 'action'>,
    params: Record<string, number>,
  ): Promise<Response> {
    return this.raw('POST', '/actions', {
      body: { board_id: row.board_id, device: row.device, action: row.action, params },
    })
  }

  // --- topology / gdb ----------------------------------------------------

  topology(): Promise<Topology> {
    return this.json<Topology>('GET', '/topology')
  }

  attachGdb(boardId: string): Promise<Response> {
    return this.raw('POST', '/gdb', { body: { board_id: boardId } })
  }

  async gdbInfo(): Promise<GdbSessionInfo | null> {
    return (await this.json<GdbSessionInfo | null>('GET', '/gdb')) ?? null
  }

  async gdbSsh(): Promise<string> {
    const body = await this.json<{ command?: string }>('GET', '/gdb/ssh')
    return body.command ?? ''
  }

  // --- SSE ---------------------------------------------------------------

  /** EventSource on `path`; adds `?viewer=` when a token is known. */
  eventSource(path: string): EventSource {
    return new EventSource(this.withViewer(`${this.base}${path}`))
  }

  /** The very first connection: no token yet; the server's first event carries it. */
  viewerSource(): EventSource {
    return new EventSource(`${this.base}/viewer`)
  }
}

export function parseProbeValue(kind: ProbeKind, value: string): number {
  if (kind === 'digital') {
    switch (value) {
      case 'High':
        return 1
      case 'Low':
        return 2
      default:
        return 0 // HiZ
    }
  }
  const n = Number(value)
  return Number.isFinite(n) ? n : 0
}

export function simIdFromPath(pathname: string): string | null {
  const path = pathname.replace(/\/+$/, '')
  if (!path || path === '/') return null
  return decodeURIComponent(path.slice(1))
}
