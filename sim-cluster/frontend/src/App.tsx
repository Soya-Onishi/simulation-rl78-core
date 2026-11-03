import { useState } from 'react'
import { simIdFromPath } from './api/client'
import './App.css'
import ActionTree from './components/ActionTree'
import GraphView from './components/graph/GraphView'
import LogPane from './components/LogPane'
import ProbeTree from './components/ProbeTree'
import TopBar from './components/TopBar'
import WiringView from './components/wiring/WiringView'
import { useSimulation } from './hooks/useSimulation'
import type { ActionRow } from './types'

function Tabs<T extends string>({
  tabs,
  value,
  onChange,
}: {
  tabs: { id: T; label: string }[]
  value: T
  onChange: (id: T) => void
}) {
  return (
    <div className="tabs" role="tablist">
      {tabs.map((t) => (
        <button
          key={t.id}
          type="button"
          role="tab"
          aria-selected={t.id === value}
          className={t.id === value ? 'tab active' : 'tab'}
          onClick={() => onChange(t.id)}
        >
          {t.label}
        </button>
      ))}
    </div>
  )
}

type MainTab = 'graph' | 'wiring'
type SideTab = 'probes' | 'actions'

function Simulation({ simId }: { simId: string }) {
  const sim = useSimulation(simId)
  const [mainTab, setMainTab] = useState<MainTab>('wiring')
  const [sideTab, setSideTab] = useState<SideTab>('probes')

  if (sim.phase === 'busy') {
    return (
      <div className="fullscreen">
        <h1>使用中です</h1>
        <p className="meta">このシミュレーションは別のブラウザで表示されています。</p>
      </div>
    )
  }
  if (sim.phase === 'destroyed') {
    return (
      <div className="fullscreen">
        <h1>破棄しました</h1>
      </div>
    )
  }

  const running = sim.status?.state === 'running'
  const invoke = async (row: ActionRow, params: Record<string, number>) => {
    await sim.guard(sim.client.invokeAction(row, params))
  }

  return (
    <div className="app">
      <div className="area-top">
        <TopBar
          simId={simId}
          status={sim.status}
          statusError={sim.statusError}
          commandError={sim.commandError}
          onCommand={(name) => void sim.command(name)}
          onDestroy={() => void sim.destroy()}
          onCsv={() => void sim.downloadCsv()}
        />
      </div>

      <main className="area-main pane">
        <Tabs
          tabs={[
            { id: 'graph', label: 'Graph' },
            { id: 'wiring', label: 'Wiring' },
          ]}
          value={mainTab}
          onChange={setMainTab}
        />
        <div className="pane-body">
          {sim.phase === 'connecting' ? (
            <p className="meta pad">接続中…</p>
          ) : mainTab === 'graph' ? (
            <GraphView
              store={sim.store}
              catalog={sim.catalog}
              watched={sim.watched}
              virtualTimeNs={sim.status?.virtual_time_ns ?? 0}
            />
          ) : (
            <WiringView
              client={sim.client}
              guard={sim.guard}
              refreshKey={(sim.status?.nodes ?? []).map((n) => n.id).join(',')}
            />
          )}
        </div>
      </main>

      <aside className="area-side pane">
        <Tabs
          tabs={[
            { id: 'probes', label: 'Probes' },
            { id: 'actions', label: 'Actions' },
          ]}
          value={sideTab}
          onChange={setSideTab}
        />
        <div className="pane-body scroll">
          {sideTab === 'probes' ? (
            <ProbeTree catalog={sim.catalog} watched={sim.watched} onChange={sim.setWatched} />
          ) : (
            <ActionTree
              actions={sim.actions}
              actionsError={sim.actionsError}
              running={running}
              onInvoke={invoke}
              onReload={() => void sim.reloadActions()}
            />
          )}
        </div>
      </aside>

      <section className="area-log">
        <LogPane logs={sim.logs} />
      </section>
    </div>
  )
}

export default function App() {
  const simId = simIdFromPath(window.location.pathname)
  if (!simId) {
    return (
      <div className="fullscreen">
        <h1>シミュレーション</h1>
        <p className="meta">URL にシミュレーション ID を指定してください（例: /{'{id}'}）。</p>
      </div>
    )
  }
  return <Simulation simId={simId} />
}
