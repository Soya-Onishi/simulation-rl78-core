# cluster-server の gdbserver

GDB RSP は `cluster-server` が `127.0.0.1` で待ち受ける。1 シミュレーションにつき 1 本。対象は RL78 ボードのみ（plant は対象外）。

## 手順

1. WebUI でシミュレーションを開き、GDB 対象ボードを選んで「接続」する。  
   このときクラスタは必ず停止する。
2. 表示されたポートと `ssh` コマンドを控える。
3. サーバと別マシンから使う場合は、先にポート転送する:

```bash
ssh -L 45231:127.0.0.1:45231 user@192.168.1.10
```

転送先は常に `127.0.0.1`（gdbserver の bind 先）。`user@host` はサーバが表示した値を使う。
4. GNU `rl78-elf-gdb` または VS Code から `127.0.0.1:<port>` に接続する。

WebUI のスタート / ストップ / リセット / `until_ns` は GDB 接続中は使えない。実行制御は GDB の continue / interrupt / step。ボードを切り替えるときは切断してから付け直す（ポートは変わり得る）。

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
- `GET /api/simulation/{id}/gdb`
- `DELETE /api/simulation/{id}/gdb`

ビューアトークンは不要。
