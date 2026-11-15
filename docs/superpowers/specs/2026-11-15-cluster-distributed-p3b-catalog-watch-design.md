# クラスタ分散 p3b — catalog / actions 結合と probe watch の fanout

Status: **approved**（実装計画へ）。  
Branch: `feature/cluster-distributed-p3b-catalog-watch`（integration `feature/cluster-distributed` から。p3a マージ済み）。  
確定ベースライン: Serena `mem:sim-cluster/distributed-design`、`docs/cluster-distributed-phases.md`、p3a マージ済み（#133）。

## 意図

p3a のあと、単一ボード向け RPC は配置先バックエンドに届くが、シミュレーション全体の一覧と probe watch はまだ `primary_backend` だけを見ている。マルチホストでは他ホストの actions・probe catalog・watch サンプルが欠ける。p3b は、ブラウザに「どの `MachineId` がどのボードを持つか」を知らせずに、これらの API を全参加ホスト分カバーする。

## 目標

- **`actions` / `probe_catalog`**: `SimRecord.per_server` の全参加ホストへ fanout し、1 本の結合リストを返す。
- **`probe_watch_get` / `put` / `clear`**: ボード配置に従いホストごとに watch 集合を適用する。get は和集合を返す。
- **`probe_watch_live`**: 粗いライブバッチの結合は `ClusterFrontend` 内で行う。SSE のカーソルは配信 `seq` ではなく **仮想時間（VT）**。重ね取りの重複は **ホストごとのローカル `seq`** で除去する。
- catalog / actions / SSE `probe` イベント本体のブラウザ向け JSON は既存 UI と互換を保つ。ライブ **カーソル** は SSE ループの責務（`CoarseLive.cursor_vt`）。ブラウザは `after` を送らない。

## 非目標

- p4 の CSV 集約・時刻ソート。
- Viewer の attach / check / detach（当面 `primary_backend` のまま）。
- Topology 取得（create 後は全バックエンドがフル topology を保持）。
- クロスホスト Digital エッジ / Digital IPC。
- `MachineId` やホスト別配置をブラウザへ公開すること。
- coarse バッチをフロントエンド全体のグローバル `seq` 空間へ付け直すこと（明示的に不採用。p1d のログとは対照）。

## アンチゴール

- マルチホスト結合やホスト別 `seq` マップを `probe_watch_sse` に置くこと（配置情報を HTTP 層へ押し込むことになる）。
- SSE やブラウザがホストローカルの `CoarseBatch.seq` をライブカーソルに使うこと（ホスト間で番号が衝突する）。
- すべり窓なしに単一 VT カーソルだけを `current_min_vt` より先へ進めること（同期 margin 内の遅れや retention 落ちで取りこぼしが定常化する）。

## 制約

- 参加・配置の根拠は `SimRecord.per_server`（p1d / p3a と同じ）。`ClusterFrontend` はすでに持つ。`probe_watch_sse` から見えるのは `dyn SimulationBackend` のみ。
- クラスタ表示用 VT = 成功した参加ホスト status の VT の min（p1d）。ボードは VT margin 内で先に進みうる。**`virtual_time_ns <= current_min_vt` のサンプルは、出すホストではすでに生成済み**とみなす。
- 現状、各バックエンドの `ProbeWatch` はローカル `next_seq` を採番し、`pending` は **件数**上限（`WATCH_PENDING_KEEP = 64`）。`live_watch` は `seq > after` を返して drain する。
- 現状、`probe_watch_sse` / `probe_event_stream` の `after` は上記 **seq** の高水位。ブラウザの `EventSource('/probes/stream')` は `after` を管理しない。

## 方針比較

| | 案 | トレードオフ |
|---|------|----------|
| **A（採用）** | VT すべり窓 + `ClusterFrontend` のホスト別 `seq` 除去。SSE カーソル = VT | バックエンド retention と `CoarseBatch` の最小 VT が必要。live 応答に `cursor_vt` を載せる |
| B | p1d 同型のみ: ホスト別 `after_seq` + フロントのグローバル `seq` を SSE 用に付け直し | 現行 `live_watch(seq)` に合うが、カーソルを VT にする選択と反対 |
| C | SSE が `HashMap<MachineId, seq>` を持つ | SSE への配置通知 / ハンドシェイクが必要。不採用 |

会話で採用: **A**。重複除去は **ホストローカル `seq` のみ**（グローバル `seq` 付け直しなし）。

## 振る舞い — `actions` / `probe_catalog`

1. `per_server` から参加 `MachineId` を clone し、I/O 前に台帳ロックを外す。
2. 各参加ホストへ並列取得（p1d の observe fanout と同型）。
3. ホスト単位の `Err` はその呼び出しでは省略（status と同じ observe 流儀）。全参加が失敗したら `Err`。
4. 成功分を `MachineId` 昇順で連結する。
5. 重複除去キー（安定）:
   - `actions`: `(board_id, device, action)`
   - `probe_catalog`: `(board, probe)`
   先勝ち（`MachineId` 昇順連結の先着）。同一キーでラベル等の中身が食い違ってもフィールド結合はせず、後着を捨てる。

## 振る舞い — `probe_watch_get` / `put` / `clear`

**put**

1. `probes: Vec<ProbeRef>` を `backend_for_board(id, probe.board)` で分割する。
2. 未知ボード → p3a と同じ `unknown_board`（put 全体を失敗）。
3. 各配置ホストへ、そのホスト分の refs だけを `probe_watch_put(id, subset)`（各 arbiter は自ホストのボードだけ watch）。
4. 並列。いずれか `Err` → 全体 `Err`（失敗のうち最小 `MachineId` を返す。control fanout と同じ）。
5. 結合した `ProbeWatchView` を返す（実際に出した subset の和、または再 get。実装者選択）。

**get**

- `probe_watch_get` を fanout。`probes` を結合し `(board, name)` で重複除去。`MachineId` 昇順、ホスト内は安定順。

**clear**

- 全参加へ `probe_watch_clear` を fanout。いずれか `Err` → 全体 `Err`。

## 振る舞い — `probe_watch_live`（中核）

### 責務分担

| 層 | 役割 |
|-------|------|
| `probe_watch_sse` / `probe_event_stream` | VT カーソル `cursor_vt` を保持。`backend.probe_watch_live(id, cursor_vt)` を呼ぶ。`probe` イベントを送出。`cursor_vt` は応答の `cursor_vt` で更新（`batch.seq` ではない）。 |
| `ClusterFrontend::probe_watch_live` | 参加ホストと `current_min_vt` を解決。取得下限を計算。fanout。**ホスト別 `seq` 除去**。batches と `cursor_vt` を返す。 |
| 各 `cluster-backend` | 一定 VT 幅の coarse バッチを保持。各バッチに最小 VT を載せる。要求された下限に重なるバッチを返す。 |

### VT カーソルと窓

定数（実装デフォルト。単位は **仮想時間** の ns）:

- `WATCH_VT_WINDOW_NS = 100_000_000`（100 ms）— 取得下限のオフセット。
- `WATCH_VT_RETAIN_NS = 1_000_000_000`（1 s）— バックエンド pending の保持幅。**窓以上であること**。

`ClusterFrontend` 内の各ポール:

1. `current_min_vt` = この sim の結合 status VT（成功ホストの min。p1d と同じ定義）。status をどの参加からも取れない → `Err`。1 台以上成功ならその min を使う（`status` API と同じ）。
2. 取得下限: `fetch_from = current_min_vt.saturating_sub(WATCH_VT_WINDOW_NS)`。  
   SSE が渡す `cursor_vt` は前回応答のカーソル（前回ポールの `current_min_vt`）。これを唯一の前進下限にして retention の外へ取り残さない。下限は常に **今回ポールの** `current_min_vt - window` に錨を置き、時間進行に対して取りこぼしが定常化しないようにする。
3. 結合後、応答の `cursor_vt = current_min_vt`（「前回までに進めた VT」が今回の min VT になる）。

SSE 初回 subscribe: `cursor_vt = 0` を渡す。下限はやはり `current_min_vt - window`（下流れは `0`）。

### ホスト別 `seq` 除去

シミュレーションごとにフロントエンドが保持:

```text
ProbeWatchAggregate {
  per_backend_seq: HashMap<MachineId, u64>,  // 最後に出したローカル seq。欠落キー ⇒ 0
}
```

ライフサイクル: `SimRecord` 作成時に用意 / sim 削除時に破棄（`LogAggregate` と同じ）。

各参加 `m`（`MachineId` 昇順）について、`fetch_from` でバックエンド live を呼ぶ（意味は下記）。返ってきた `CoarseBatch` をホスト内順で見て、`batch.seq <= per_backend_seq[m]` ならスキップし、そうでなければ出して `per_backend_seq[m] = batch.seq` とする。

SSE への送出順: 参加 `MachineId` 昇順、その中で各ホストのバッチ順。p3b では `min_virtual_time_ns` によるホスト横断ソートはしない（CSV の時刻ソートは p4）。

SSE 向けに新しいグローバル `seq` は付けない。

### バックエンド pending / ワイヤ

1. `CoarseBatch` に `min_virtual_time_ns: u64` を追加 = `samples[].virtual_time_ns` の min（空バッチは実装者選択 — `0` にするか retention 対象外）。
2. 件数だけの保持を VT 保持に置き換え: `pending` 先頭について `front.min_virtual_time_ns + WATCH_VT_RETAIN_NS < sim.virtual_time_ns`（ローカル sim VT）なら落とす。安全のための件数上限を二次的に残してよい。
3. live のクエリ / trait 引数 `after` の意味は **VT 取得下限**（`fetch_from`）であり seq ではない。バックエンドは `min_virtual_time_ns >= fetch_from` の pending バッチを返す（seq だけで drain し切らない）。重ねポールが効くよう、マッチ分をすぐに破棄しきる現行 drain から、retention まで残す形へ変える。

### 戻り型（空ポールでもカーソルが必要）

`probe_watch_live` は **`batches` が空でも `cursor_vt` を返す**必要がある（走行中で新規サンプルなし）。バックエンド trait / 内部 HTTP JSON を素の `Vec<CoarseBatch>` から例えば次へ変更する:

```text
CoarseLive { cursor_vt: u64, batches: Vec<CoarseBatch> }
```

ブラウザは従来どおり SSE の `probe` イベント（バッチ JSON）だけを読む。SSE ループは `CoarseLive.cursor_vt` を保持し、`max(batch.seq)` は使わない。

`CoarseBatch.seq` はホストローカルのまま（SSE JSON に載ってもよい）。現行 UI は見ていない。

### 部分失敗

- live fanout: そのポールで `Err` の参加ホストは省略（observe 流儀）。除去状態の更新は成功ホストのみ。
- 全参加が失敗 → `Err`。

### リセット

クラスタ VT が戻る（reset）とき、その sim の `ProbeWatchAggregate` をクリアする。ブラウザ側ストアは status 巻き戻し等で既存どおりクリアされる。`ClusterFrontend` は `reset` 成功時にホスト別 seq マップをリセットする（台帳上の reset を観測する箇所と同じタイミング）。

## 再接続（確定）

`ProbeWatchAggregate` は simulation 単位で 1 つ共有する。切断後に同じ sim で `/probes/stream` を開き直しても、すでに aggregate が進んだ分は VT 窓内でも再送しない（新しい `seq` が来るまで SSE は一時的に空になりうる）。その一時的な空は許容する。接続ごとの除去状態は持たない。

## マージ後の docs / memory

- `docs/cluster-distributed-phases.md` で p3b を merged にし、p3a #133 を記す。
- Serena `mem:sim-cluster/distributed-design` の phases と、live-watch の短い注記（VT カーソル、`ClusterFrontend` のホスト別 seq 除去）を更新する。

## 実装者に残す選択

- 構造体名（`CoarseLive` など）と内部 HTTP のパス / クエリ名。
- VT retention に併用する二次的な件数上限。
- 空バッチの `min_virtual_time_ns`。
- put 応答: subset 和 vs 再 get。

## この版で確定した実装者向け既定

- **窓 / 保持:** `WATCH_VT_WINDOW_NS = 100ms`、`WATCH_VT_RETAIN_NS = 1s`。いずれも **仮想時間**。
- **重複除去:** ホストローカル `seq` のみ（グローバル `seq` 付け直しなし）。
- **SSE:** カーソルは `CoarseLive.cursor_vt`（= `current_min_vt`）。`batch.seq` では進めない。
- **ブランチ:** p3a 取り込み後の integration から `feature/cluster-distributed-p3b-catalog-watch`。
- **検証:** `cluster_frontend_tests` のモック複数バックエンド単位テスト（結合、put 分割、live 除去 + カーソル）。マージにマルチプロセス smoke は不要。
- **再接続:** sim 共有 aggregate。進行分のみ。一時的な空は許容。

## この文書で閉じた決定

- live 結合は `ClusterFrontend` に置き、`probe_watch_sse` には置かない。
- SSE / live カーソルは VT。取得下限は min VT − 窓。重ね取りの除去だけホストローカル `seq`。
- 方針 A を B/C より採用。
- `ProbeWatchAggregate` は sim 共有。再接続は進行分のみ（窓の再送なし）。一時的な空は許容。
