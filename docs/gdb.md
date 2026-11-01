# cluster-server の gdbserver

GDB RSP は web（HTTP と同じ `cluster-server` プロセス、専用スレッド）が `127.0.0.1` で待ち受ける。1 シミュレーションにつき listen は 1 本。対象は RL78 ボードのみ（plant は対象外）。`ClusterGdbTarget` は `SimulationBackend` だけを呼ぶ。UDS inspect と iceoryx の Step は `LocalBackend` → worker の先にある。

## 手順

1. WebUI でシミュレーションを開き、GDB 対象ボードを選んで「接続」する。  
   このときクラスタは必ず停止する。ポートが出ただけでは WebUI の start は止まらない。
2. 表示されたポートと `ssh` コマンドを控える。`GET /gdb` と status の `gdb` は `board_id` / `host` / `port` / `user` / `connected` だけを返す。`ssh` 文は受信側が `ssh -L {port}:127.0.0.1:{port} {user}@{host}` で組み立てる。`host` は `cluster-server --listen` の IPv4（`0.0.0.0` は `127.0.0.1`）。
3. サーバと別マシンから使う場合は、先にポート転送する:

```bash
# cluster-server --listen 192.168.1.10:8090 のとき
ssh -L 45231:127.0.0.1:45231 user@192.168.1.10
```

転送先は常に `127.0.0.1`（gdbserver の bind 先）。`user@host` は画面に出た値を使う。
4. GNU `rl78-elf-gdb` または VS Code から `127.0.0.1:<port>` に接続する。

WebUI のスタート / ストップ / リセット / `until_ns` は、GDB クライアントが TCP でつながっているときだけ使えない。listen 中でも未接続なら WebUI から動かす。実行制御は GDB の continue / interrupt / step。未接続の再 `POST /gdb` は古い listen を閉じて作り直す（ポートは変わり得る）。接続中の再 `POST` は 409。シミュレーション削除は listen を閉じてから消す。

continue / stop / step は arbiter 経由。step は対象ノードが 1 命令進んだあと、他ノード（plant 含む）がその仮想時間までキャッチアップする。メモリ / レジスタ / ブレークポイントはノード上の inspect UDS。`g` パケットの並びは対象 CPU クレート（RL78 なら `hw/cpus/rl78`）が組み立て、cluster はバイト列と PC だけを転送する。

## VS Code

`${input:gdbPort}` に WebUI のポートを入れる。`preLaunchTask` の出力は inputs に渡せない。

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "name": "RL78 sim",
      "type": "gdb",
      "request": "launch",
      "program": "${workspaceFolder}/guest.elf",
      "cwd": "${workspaceFolder}",
      "MIMode": "gdb",
      "miDebuggerPath": "rl78-elf-gdb",
      "miDebuggerServerAddress": "127.0.0.1:${input:gdbPort}"
    }
  ],
  "inputs": [
    {
      "id": "gdbPort",
      "type": "promptString",
      "description": "WebUI に表示された gdb ポート"
    }
  ]
}
```

## HTTP

- `POST /api/simulation/{id}/gdb` body `{ "board_id": "mcu-a" }`
- `GET /api/simulation/{id}/gdb` — `board_id` / `host` / `port` / `user` / `connected`（`ssh` 文は含まない）
- `DELETE /api/simulation/{id}/gdb`

ビューアトークンは不要。
