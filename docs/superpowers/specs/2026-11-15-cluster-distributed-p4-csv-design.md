# クラスタ分散 p4 — CSV のマルチホスト集約と時刻ソート

Status: **approved**（実装へ）。  
Branch: `feature/cluster-distributed-p4-csv`（integration `feature/cluster-distributed` から。p3b マージ済み #134）。  
確定ベースライン: Serena `mem:sim-cluster/distributed-design`、`docs/cluster-distributed-phases.md`、会話で確定した合意（下記）。

## 意図

p3b までで actions / probe catalog / watch は全参加ホストをカバーするが、CSV エクスポートはまだ `ClusterFrontend` が台帳 `SimRecord.per_server` の先頭 `MachineId` 1 台へ委譲している。マルチホストでは他ホスト上のボード由来の行が CSV に出ない。p4 は、公開 HTTP のストリーミング形を保ったまま、開始・取得・状態確認を全参加ホストへ広げ、**ブラウザへ流れる CSV 全体**を仮想時刻順に保って返す。

## 目標

- **`csv_start`**: 参加ホスト全部に同一の時刻範囲（`ProbeRange`）で開始する。フロントが `request_id` を 1 つ発行し、全ホストへ同じ値を渡す。返る `probes`（行ラベル用メタデータ）は全ホスト分を 1 本に結合する。
- **`csv_poll` / `csv_status`**: 同じ `request_id` で全参加ホストへ fanout する。フロントがホスト別キューを持ち、**ポールをまたいでも** `virtual_time_ns` 順が崩れないよう出してよい行だけを返す。`done` / `failed` / `truncated` は下記の集約規則。
- 公開 HTTP（`GET .../probes.csv` と csv-status）のストリーミング変換は現状維持。ホスト横断の結合・順序制御は `ClusterFrontend` に置く。

## 非目標

- Viewer の attach / check / detach（当面 `primary_backend` のまま）。
- Topology 取得。
- クロスホスト Digital エッジ / Digital IPC。
- `MachineId` やホスト別配置をブラウザへ公開すること。
- CSV 行フォーマット（列名・エスケープ）の変更。
- ホスト側ボード IPC や arbiter 内 CSV ジョブの再設計（単一ホスト内の既存 `start_csv` / `poll_csv_job` を、外部から渡す `request_id` に対応させる変更に留める）。
- 部分開始成功後の自動キャンセル API（下記「意図的な制限」）。

## アンチゴール

- 公開 HTTP / `csv_event_stream` に `SimRecord.per_server` やホスト別 id 対応表を持ち込むこと。
- ホストごとに別 `request_id` を採番し、フロントの対応表だけで束ねること（今回不採用の案 2 / 案 3）。
- 先頭ホストの `probes` だけを使い、他ホスト行の board / 表示名を欠落させたまま出荷すること。
- **1 回の `csv_poll` 応答内だけ**をソートし、ポール間で時刻順が逆転した CSV を許容すること（不採用。全体順を保証する）。

## 制約

- 参加の根拠は `SimRecord.per_server`（p1d / p3a / p3b と同じ）。
- 現状の公開フロー: `probes_csv` が `csv_start` → 初回 `csv_poll` → `csv_event_stream`（20ms 間隔で `csv_poll`）。CSV テキスト化は HTTP 層が `probes` と `ProbeRecord` から行う。
- 現状の内部ワイヤ: `POST .../probes/csv/start` のボディは `ProbeRange` のみ。応答 `CsvStartBody { request_id, probes }`。各ホストは `next_probe_request` で id を採番している。
- 各ホストの `csv_job` はシミュレーションあたり高々 1 つ。ビジー時は `409 conflict`。デッドラインは 60 秒。
- ホスト側 `poll_csv_job` は `pending` を `take` して返す（取り出しは消費）。
- 行ラベル: `csv_record_line` は `probes` を `probe_id` で検索し、board 列と probe 列を埋める。各ホストの `probes` には自ホスト割当ボード分しか載らない。

## 方針比較

| | 案 | トレードオフ |
|---|------|----------|
| **1（採用）** | `csv_start` と内部開始ボディに `request_id` を追加。フロントが 1 つ発行して全ホストへ渡す。poll/status は同一 id で fanout し、フロントで順序付き出庫＋フラグ集約 | 内部 API / trait の破壊的変更。合意どおり |
| 2 | ホストが従来どおり採番。フロントはホスト→id 対応表のみ | ホスト開始 API を変えにくいが、「ホストへ同じ request_id を渡す」合意と不一致 |
| 3 | フロントに集約ジョブ用ストアを追加し、ホストは既存 API のまま | ホスト契約は温存だが状態が増え、共通 request_id 合意も満たさない |

会話で採用: **案 1**。`probes` 結合は選択肢 B（全ホスト結合、MachineId 昇順・同一 `probe_id` は先勝ち）。

全体の時刻順については、ポール内ソートのみ（後から指摘されうる）を避け、**k-way マージ（ウォーターマーク）**でポール横断の順序を保証する（会話で (b) を採用。実現方法は実装者向け既定）。

## 振る舞い — `csv_start`

1. `per_server` から参加 `MachineId` を clone し、I/O 前に台帳ロックを外す。
2. `ClusterFrontend` が `request_id` を 1 つ発行する（フロント全体で単調増加するカウンタ）。
3. 各参加ホストへ並列に `csv_start(id, range, request_id)`（同一 `ProbeRange`・同一 `request_id`）。
4. いずれか `Err` → 全体 `Err`（失敗のうち最小 `MachineId` のエラーを返す。control fanout と同じ）。
5. 成功分の `probes` を `MachineId` 昇順で連結する。重複除去キーは `probe_id`。先勝ち（後着を捨て、フィールド結合しない）。
6. **全ホスト成功のときだけ**、その sim 向けのフロント側マージ状態（下記 `CsvMergeState`）を新規作成して置き換える。参加 `MachineId` 集合はこの開始時点で **固定** する（以降の poll は台帳を再読して参加者を増やさない）。いずれか失敗した開始では、既存の `CsvMergeState` を触らない。
7. 戻り値は `(request_id, merged_probes)`。返す `request_id` は発行した値。
8. 同一 sim で前回ジョブがホスト上でまだビジーなあいだに再度開始すると、成功済みホストが `409 conflict` を返しうる（上記の意図的制限）。フロントはそれを全体 `Err` として返す。

### 意図的な制限 — 部分開始成功後のキャンセルなし

成功ホストのジョブを自動キャンセルしない（ホスト側に cancel API が無い）。成功ホストには最大 60 秒ビジーなジョブが残り、すぐ再試行するとそのホストが `409 conflict` を返しうる。

**これは意図的な制限である。** 実装では開始ファンアウト付近に、次を満たすコメントを残すこと。

- 部分成功時に cancel しないこと
- 成功ホストが最大 60 秒 `csv export is busy` になりうること
- Bugbot / レビューが「直し漏れ」と見なさないよう、設計で許容した制限であること

cancel や同一 `request_id` 再開始を足す変更は、この制限を見直す設計変更として扱う。

### ホスト側（各 cluster-backend）

1. 内部開始ボディから `request_id` と `ProbeRange` を読む。
2. 既存どおりカタログ必須・ビジー検査。
3. ジョブの `request_id` に受け取った値を使う（`next_probe_request` で採番しない）。
4. 後続の probe query / action 採番と衝突しないよう、`sim.next_request_id = max(sim.next_request_id, request_id.saturating_add(1))` とする。
5. 以降のボードへの CSV 問い合わせ・`csv_job` ライフサイクルは現状どおり。

## 振る舞い — `csv_poll`（全体時刻順）

### フロント側状態

シミュレーションごと（`LogAggregate` / `ProbeWatchAggregate` と同型の寿命）:

```text
CsvMergeState {
  request_id: u64,
  hosts: HashMap<MachineId, HostCsvBuf>,
}
HostCsvBuf {
  queue: VecDeque<ProbeRecord>,  // まだ公開側へ出していない行
  done: bool,
  failed: bool,
  truncated: bool,
}
```

- `csv_start` 成功時に作成（参加 `MachineId` 分の空バッファ）。
- sim 削除時に破棄。`request_id` 不一致の poll は `not_found`。

### ホスト側フラグ（既存挙動・依存）

現行 `ingest_csv_replies` では、期限切れや reply エラーで `failed = true` のとき **同時に `done = true`** になる。したがって k-way マージは、失敗ホストを「永遠に待ち」しない（`done` が立つ）。`truncated` は last チャンクでも立ちうるが、ジョブ完了は `expected` 空または failed 経路の `done` に従う。

### 1 回の `csv_poll`

1. `CsvMergeState` に固定した参加ホストへ同一 `request_id` で並列 fanout（台帳の再読で参加者を変えない）。
2. **いずれかのホストが輸送・RPC の `Err`:**
   - このポールで他ホストから受け取った `records` は **公開側へ出さず捨てる**（ホスト側ではすでに消費済み）。
   - 全体 `Err`（最小 `MachineId`）。
   - 当該 `CsvMergeState` を破棄し、以降の同じ `request_id` の poll は `not_found`（公開 HTTP はストリーム中断）。
   - 不完全な CSV を黙って完走させない。
3. 全ホストが `Ok` のとき: 各ホストの `records` をそのホストの `queue` 末尾へ追加し、`done` / `failed` / `truncated` フラグを更新する。
4. **出庫（k-way マージ）** — 次を繰り返して今回返す `records` を作る:
   1. まだ `done` でないホストのうち、`queue` が空のものが 1 台でもいれば停止（そのホストの次データ待ち。ウォーターマーク）。
   2. そうでなければ、空でない `queue` を持つホストの先頭行のうち、`(virtual_time_ns, MachineId)` が最小のものを 1 行 pop して出力へ足す（安定: 同時刻は MachineId 昇順、同ホスト内は到着順）。
   3. 全ホストが `done` かつ全 `queue` が空なら、出力を確定してループ終了。
5. 返却フラグ:
   - `done`: 全ホスト `done` **かつ** 全 `queue` が空。
   - `failed`: いずれかのホスト `failed`。
   - `truncated`: いずれかのホスト `truncated`。

これでポールをまたいでも、先に出た行の `virtual_time_ns` が後続行より大きくならない（全体の非減少順）。

全件バッファしてから一度に出す方式は採らない（レイテンシとピークメモリのため。実装者向け既定）。

## 振る舞い — `csv_status`

1. `CsvMergeState` に固定した参加ホストへ同一 `request_id` で `csv_status` を fanout（行は消費しない）。
2. いずれか輸送 `Err` → 全体 `Err`。
3. フラグ集約は poll と同じ定義。ただしフロントに未出庫の `queue` が残っている間は `done = false`（ホストが先行して done でも、公開側のストリームが終わるまでは done にしない）。
4. 返却の `records` は空（現状の status 契約を維持）。

## 公開 HTTP

- `probes_csv` / `csv_event_stream` / `probes_csv_status` は、引き続き `dyn SimulationBackend` の `csv_start` / `csv_poll` / `csv_status` だけを呼ぶ。
- ストリームのヘッダ付与・`range.contains` 再フィルタ・20ms ポール間隔は現状維持。
- `x-probe-csv-id` はフロントが発行した共通 `request_id`。
- poll が `Err` を返したときのストリーム終了は現状どおり（取り出し済み行の再送はしない。会話で (a) を採用）。

## API / ワイヤ変更

- `SimulationBackend::csv_start` に `request_id: u64` 引数を追加する。戻りは従来どおり `(u64, Vec<ProbePointName>)`。
- 内部 `POST /internal/simulations/{id}/probes/csv/start` のボディを、`ProbeRange` 単体から **`request_id` を含む開始リクエスト**へ変更する（フィールド配置は実装者選択）。
- `TcpipBackend` / `LocalBackend` / テストモックを追随させる。
- 公開 HTTP のパスとクエリ（`from_ns` / `to_ns` / `viewer`）は変えない。

## マージ後の docs / memory

- `docs/cluster-distributed-phases.md` で p3b を merged（#134）と記し、p4 を this branch / 完了後 merged に更新する。
- Serena `mem:sim-cluster/distributed-design` の phases と CSV 注記を更新する（共通 `request_id`、k-way マージによる全体時刻順、`probes` 結合、部分開始の意図的制限）。

## 実装者に残す選択

- 開始ボディの JSON 形（フラット vs `range` ネスト）。
- `CsvMergeState` / カウンタの型の置き場。
- 単体テストのモック構成（共通 request_id、probes 結合、ポール横断の時刻順、フラグ、部分開始時のコメント存在はレビュー観点）。

## この版で確定した実装者向け既定

- **方針:** 案 1。`probes` は全ホスト結合（MachineId 昇順・`probe_id` 先勝ち）。
- **全体時刻順:** k-way マージ（未完了ホストの空キューで出庫停止）。全件溜めてから一括放出はしない。
- **参加者:** `csv_start` 成功時に `CsvMergeState` へ固定。poll/status はその集合へ fanout。
- **再開始:** 全ホスト成功時のみ state を置き換え。失敗した開始は旧 state を維持。ホストビジーによる `409` は全体 `Err`。
- **開始失敗:** 1 台でも `Err` → 全体 `Err`。成功ホストの自動キャンセルなし（**意図的。コードコメント必須**）。
- **poll 輸送失敗:** 1 台でも `Err` → 全体 `Err`。このポールで受けた行は捨てる。`CsvMergeState` 破棄。再送しない。ストリーム中断。
- **フラグ:** `done` = 全ホスト done かつフロント queue 空、`failed` / `truncated` = いずれか true。ホストは failed 時に done も立てる（既存）。
- **ホスト採番:** 受け取った `request_id` をジョブに使い、`next_request_id` を `max(現行, request_id+1)` に進める。
- **フロント採番:** **単一** `simulation-frontend` プロセス前提の単調増加 `AtomicU64`（1 始まりでよい）。フロント複数レプリカは対象外。
- **ブランチ:** p3b 取り込み後の integration から `feature/cluster-distributed-p4-csv`。
- **検証:** `cluster_frontend_tests` のモック複数バックエンド単位テスト（共通 request_id の伝播、probes 結合、ポール横断の時刻順、フラグ集約）。マージにマルチプロセス smoke は不要。

## この文書で閉じた決定

- 全ホスト同一時刻範囲。フロント発行の共通 `request_id` をホストへ渡す。
- **CSV 全体**の `virtual_time_ns` 非減少（ポール横断）。実現は k-way マージ。
- poll 輸送 `Err` 時は取り出し済み行を捨て、ストリーム中断（再送なし）。
- 部分開始成功後の cancel なし・最大 60 秒ビジーは **意図的な制限**（コードコメント必須）。
- 行ラベル用 `probes` は全ホスト結合（選択肢 B）。
- 公開 HTTP ストリームは現状維持。結合は `ClusterFrontend`。
- 実装切り方は案 1（開始 API に `request_id` を追加）。
- 参加者固定・再開始時の state 置き換え規則・単一フロント前提の採番。
