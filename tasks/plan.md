# Implementation Plan: クラスタ分散 p3b（catalog / watch）

## Overview

設計: `docs/superpowers/specs/2026-11-15-cluster-distributed-p3b-catalog-watch-design.md`（approved）。  
`actions` / `probe_catalog` の結合、`probe_watch_*` の fanout、および VT カーソル＋ホスト別 seq 除去による `probe_watch_live` 結合を `ClusterFrontend` に実装する。ブランチは `feature/cluster-distributed-p3b-catalog-watch`（integration `feature/cluster-distributed` から）。

## Architecture Decisions

- 結合・除去はすべて `ClusterFrontend`。SSE は `CoarseLive.cursor_vt` だけを覚える。
- バックエンド live の `after` は VT 取得下限。`CoarseBatch.min_virtual_time_ns` と VT retention（1s）を追加。
- `ProbeWatchAggregate` は sim 共有。再接続の一時的な空は仕様どおり許容。
- 実装者既定: 空バッチの `min_virtual_time_ns = 0`、put は subset 和を返す、二次件数上限は現行 64 を安全弁として残す。

## Task List

### Phase 1: ワイヤ / バックエンド pending

- [ ] Task 1: `CoarseBatch` に `min_virtual_time_ns`、`CoarseLive { cursor_vt, batches }` を導入し、trait / Local / Tcpip / internal HTTP / モックを更新する
- [ ] Task 2: バックエンド `ProbeWatch` の VT retention と、`live_watch(fetch_from)`（`min_virtual_time_ns >= fetch_from`、seq だけで drain し切らない）に変更する
- [ ] Task 3: `probe_watch_sse` / `probe_event_stream` を `CoarseLive.cursor_vt` 基準に切り替え、`batch.seq` ではカーソルを進めない

### Checkpoint: Phase 1

- [ ] `cargo test -p sim-cluster --lib` で既存＋変更分が通る（単一バックエンド経路）

### Phase 2: ClusterFrontend 結合

- [ ] Task 4: `actions` / `probe_catalog` の参加ホスト fanout・結合・キー重複除去
- [ ] Task 5: `probe_watch_get` / `put` / `clear` の配置分割・fanout
- [ ] Task 6: `ProbeWatchAggregate`（ホスト別 seq）＋ `probe_watch_live`（min VT・窓・除去・`cursor_vt`）
- [ ] Task 7: create 時 aggregate 用意、destroy / reset 成功時クリア

### Checkpoint: Phase 2

- [ ] モック複数バックエンドの単体テスト（結合・put 分割・live 除去＋カーソル・再接続で既出 seq は再送しない）
- [ ] `cargo test -p sim-cluster --lib`

### Phase 3: 文書

- [ ] Task 8: `docs/cluster-distributed-phases.md` と Serena `mem:sim-cluster/distributed-design` を p3b 実装中／完了に合わせて更新

### Checkpoint: Complete

- [ ] 設計の受け入れ条件を満たす
- [ ] code-review 後に PR

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| live 戻り型変更で内部 HTTP / モック漏れ | Med | Task 1 で全 `SimulationBackend` 実装を一括更新しコンパイルで検知 |
| VT retention と件数上限の相互作用 | Low | 二次 cap=64 を残し、単体で窓内再取得を確認 |
| status 取得失敗時の live | Low | 設計どおり ≥1 status 成功で min VT。全失敗は Err |

## Open Questions

なし（再接続の空は設計で確定済み）。
