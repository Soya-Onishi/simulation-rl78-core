import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { ViewerClient } from '../api/client'
import type {
  ActionRow,
  LogEvent,
  Phase,
  ProbeBatch,
  ProbeCatalogItem,
  StatusEvent,
} from '../types'
import { probeKey } from '../types'
import { ProbeStore } from './probeStore'

const MAX_LOGS = 2000
const VIEWER_GIVE_UP_MS = 8000
const SHOWN_LEVELS = new Set(['error', 'warn', 'info'])

function messageOf(err: unknown): string {
  return err instanceof Error ? err.message : String(err)
}

export function useSimulation(simId: string) {
  const client = useMemo(() => new ViewerClient(simId), [simId])
  const store = useMemo(() => new ProbeStore(), [])

  const [phase, setPhase] = useState<Phase>('connecting')
  const [status, setStatus] = useState<StatusEvent | null>(null)
  const [statusError, setStatusError] = useState('')
  const [commandError, setCommandError] = useState('')
  const [logs, setLogs] = useState<LogEvent[]>([])
  const [catalog, setCatalog] = useState<ProbeCatalogItem[]>([])
  const [watched, setWatchedState] = useState<Set<string>>(() => new Set())
  const [actions, setActions] = useState<ActionRow[]>([])
  const [actionsError, setActionsError] = useState('')

  const catalogRef = useRef<ProbeCatalogItem[]>([])
  const goBusyRef = useRef<() => void>(() => {})
  const destroyedRef = useRef<() => void>(() => {})
  const loadActionsRef = useRef<() => Promise<void>>(async () => {})

  /** Runs `p`; on viewer_busy switches to the busy screen, otherwise rethrows. */
  const guard = useCallback(
    async <T,>(p: Promise<T>): Promise<T> => {
      try {
        return await p
      } catch (err) {
        if (client.isBusy(err)) goBusyRef.current()
        throw err
      }
    },
    [client],
  )

  useEffect(() => {
    let opened = false
    let closed = false
    let lastState: string | null = null
    let lastVt = 0
    let actionPoll: ReturnType<typeof setInterval> | null = null
    let actionCount = 0
    let statusSrc: EventSource | null = null
    let logSrc: EventSource | null = null
    let probeSrc: EventSource | null = null
    let viewerSrc: EventSource | null = null

    const closeLive = () => {
      statusSrc?.close()
      logSrc?.close()
      probeSrc?.close()
      statusSrc = logSrc = probeSrc = null
    }
    const closeAll = () => {
      closed = true
      if (actionPoll) clearInterval(actionPoll)
      actionPoll = null
      closeLive()
      viewerSrc?.close()
      viewerSrc = null
    }

    const loadActions = async () => {
      try {
        const rows = await client.actions()
        actionCount = rows.length
        setActions(rows)
        setActionsError('')
      } catch (err) {
        if (client.isBusy(err)) goBusyRef.current()
        else setActionsError(messageOf(err))
      }
    }
    loadActionsRef.current = loadActions

    const openProbeStream = () => {
      probeSrc?.close()
      probeSrc = client.eventSource('/probes/stream')
      probeSrc.addEventListener('probe', (ev) => {
        try {
          const batch = JSON.parse((ev as MessageEvent<string>).data) as ProbeBatch
          // TODO: this is the coarse 100 ms stream; fetch /probes/samples for full-resolution history.
          store.append(batch.samples ?? [])
        } catch {
          // ignore malformed event
        }
      })
    }

    let catalogReady = false
    let catalogConnecting = false
    const connectProbes = async () => {
      if (closed || catalogReady || catalogConnecting) return
      catalogConnecting = true
      try {
        try {
          await client.clearWatch()
        } catch (err) {
          if (client.isBusy(err)) {
            goBusyRef.current()
            return
          }
        }
        if (closed) return
        store.clear()
        try {
          const items = await client.probeCatalog()
          if (closed) return
          catalogReady = true
          catalogRef.current = items
          setCatalog(items)
          setWatchedState(new Set())
          setCommandError('')
          openProbeStream()
        } catch (err) {
          if (closed) return
          if (client.isBusy(err)) goBusyRef.current()
          else if (!(err instanceof Error && err.message.includes('not ready'))) {
            setCommandError(messageOf(err))
          }
        }
      } finally {
        catalogConnecting = false
      }
    }

    const openStatusAndLog = () => {
      statusSrc?.close()
      logSrc?.close()
      statusSrc = client.eventSource('/status')
      statusSrc.addEventListener('status', (ev) => {
        try {
          const snap = JSON.parse((ev as MessageEvent<string>).data) as StatusEvent
          if (snap.virtual_time_ns < lastVt) store.clear() // reset: time went backwards
          lastVt = snap.virtual_time_ns
          setStatus(snap)
          setStatusError(snap.state === 'exited' ? 'arbiter has exited' : '')
          if (snap.state === 'running' && lastState !== 'running') {
            void loadActions()
            if (!catalogReady) void connectProbes()
          }
          lastState = snap.state
        } catch {
          // ignore malformed event
        }
      })
      statusSrc.onerror = () => setStatusError('status stream disconnected')

      logSrc = client.eventSource('/log')
      logSrc.addEventListener('log', (ev) => {
        try {
          const log = JSON.parse((ev as MessageEvent<string>).data) as LogEvent
          if (!SHOWN_LEVELS.has(log.level || 'info')) return
          setLogs((prev) => {
            const next = prev.length >= MAX_LOGS ? prev.slice(prev.length - MAX_LOGS + 1) : prev.slice()
            next.push(log)
            return next
          })
        } catch {
          // ignore malformed event
        }
      })
    }

    goBusyRef.current = () => {
      closeAll()
      client.token = null
      setPhase('busy')
    }
    destroyedRef.current = () => {
      closeAll()
      setPhase('destroyed')
    }

    const giveUp = setTimeout(() => {
      if (!opened) goBusyRef.current()
    }, VIEWER_GIVE_UP_MS)

    viewerSrc = client.viewerSource()
    viewerSrc.addEventListener('viewer', (ev) => {
      if (closed) return
      const prev = client.token
      client.token = (ev as MessageEvent<string>).data
      if (!opened) {
        opened = true
        clearTimeout(giveUp)
        setPhase('ready')
        openStatusAndLog()
        void connectProbes()
        void loadActions()
        actionPoll = setInterval(() => {
          if (lastState === 'exited') return
          if (actionCount === 0) void loadActions()
          if (!catalogReady) void connectProbes()
        }, 2000)
        return
      }
      if (prev !== client.token) {
        // Token rotated: live streams must be re-opened with the new one.
        openStatusAndLog()
        if (catalogReady) openProbeStream()
        else void connectProbes()
      }
    })

    const onPageHide = () => {
      closeAll()
      client.clearWatch(true).catch(() => {})
    }
    window.addEventListener('pagehide', onPageHide)

    return () => {
      clearTimeout(giveUp)
      window.removeEventListener('pagehide', onPageHide)
      closeAll()
    }
  }, [client, store])

  const run = useCallback(
    async (fn: () => Promise<unknown>) => {
      try {
        await guard(fn())
        setCommandError('')
      } catch (err) {
        if (!client.isBusy(err)) setCommandError(messageOf(err))
      }
    },
    [client, guard],
  )

  const command = useCallback(
    (name: 'start' | 'stop' | 'reset') =>
      run(async () => {
        await client.command(name)
        if (name === 'reset') store.clear()
      }),
    [client, run, store],
  )

  const destroy = useCallback(async () => {
    if (!window.confirm('このシミュレーションを破棄しますか？')) return
    try {
      await guard(client.destroy())
      destroyedRef.current()
    } catch (err) {
      if (!client.isBusy(err)) setCommandError(messageOf(err))
    }
  }, [client, guard])

  const downloadCsv = useCallback(() => run(() => client.downloadCsv()), [client, run])

  const setWatched = useCallback(
    (keys: Set<string>) => {
      setWatchedState(keys)
      store.retainOnly(keys)
      const probes = catalogRef.current
        .filter((item) => keys.has(probeKey(item.board, item.probe)))
        .map((item) => ({ board: item.board, name: item.probe }))
      void run(() => client.putWatch(probes))
    },
    [client, run, store],
  )

  const reloadActions = useCallback(() => loadActionsRef.current(), [])

  return {
    client,
    store,
    phase,
    status,
    statusError,
    commandError,
    setCommandError,
    logs,
    catalog,
    watched,
    setWatched,
    actions,
    actionsError,
    reloadActions,
    command,
    destroy,
    downloadCsv,
    guard,
  }
}
