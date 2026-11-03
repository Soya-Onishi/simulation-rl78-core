import type { GdbSessionInfo } from '../../types'

export interface GdbDialogState {
  info: GdbSessionInfo | null
  ssh: string
  /** e.g. a 409 on attach; the existing session (if any) stays shown. */
  error?: string
}

export default function GdbDialog({ state, onClose }: { state: GdbDialogState; onClose: () => void }) {
  const { info, ssh, error } = state
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" role="dialog" aria-label="GDB" onClick={(ev) => ev.stopPropagation()}>
        <h3>GDB</h3>
        {error && <p className="err">{error}</p>}
        {info ? (
          <>
            <div className="meta">ボード: {info.board_id}</div>
            <div>
              接続先: <code className="select-all">{`${info.host}:${info.port}`}</code>
            </div>
            {ssh && (
              <div>
                SSH:
                <pre className="select-all ssh-cmd">{ssh}</pre>
              </div>
            )}
          </>
        ) : (
          !error && <p className="meta">セッションはありません。</p>
        )}
        <div className="modal-actions">
          <button type="button" className="text-btn" onClick={onClose}>
            閉じる
          </button>
        </div>
      </div>
    </div>
  )
}
