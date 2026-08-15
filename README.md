# simulation-rl78-core

RL78 シミュレーション基盤（[issue #1](https://github.com/Soya-Onishi/simulation-rl78-core/issues/1)）。
TCG コアは [tlib](https://github.com/Soya-Onishi/tlib)（`rl78` ブランチ）、その上位を Rust で組み立てる。

設定ファイルは使わない。マシン記述はすべて Rust コード。

## クレート

| クレート | 役割 |
|----------|------|
| `sim-kernel` | 仮想時計・イベントキュー・`MemoryBus` / `MemoryMapped`・型付き `Wire` / `SourcePort`・start/stop/quit・検査 API |
| `rl78-core` | RL78 コア（tlib 静的リンク、CPU ラッパ、最小メモリマップ、Magic probe、ELF） |
| `simulation-rl78-core` | 薄い CLI（bin のみ）。シミュレーション本体は別スレッド |

## 開発順（issue #1）

| 段階 | 内容 | 状態 |
|------|------|------|
| **A** | Workspace / クレート骨格 | 完了 |
| **B** | sim-kernel 実行核 | 完了 |
| **C** | tlib submodule + cmake + 最小 FFI | 完了 |
| **D** | tlib メモリ CB → Bus、ROM/RAM、未マップ | 本ブランチ |
| **E** | ELF + Magic probe 接続 + `minimal_machine` | 本ブランチ |
| **F** | CLI `start` / `stop` / `quit`（任意 ELF 引数） | 本ブランチ |
| **G** | 最小ゲスト ELF で Magic 出力スモーク | 本ブランチ |

## ビルド / テスト

初回は tlib submodule が必要:

```bash
git submodule update --init --recursive
cargo test
cargo run
```

`rl78-core` の `build.rs` が [tlib](https://github.com/Soya-Onishi/tlib)（`rl78` ブランチ）を CMake でビルドし、`libtlib.a` を静的リンクする。要: `cmake`、C コンパイラ、`pthread`。

CLI は標準入力から `start` / `stop` / `quit` を受け付ける。任意でゲスト ELF を引数に渡せる:

```bash
cargo run -- path/to/guest.elf
```

Magic probe はゲスト物理 `0xF0000`（`MOV !addr16, #imm` が `addr16 | 0xF0000` に到達）。

## 配線（コード記述）

ピン結線は設定ファイルを使わない。`Wire<T, Sinks, Sources>`（typenum）を 1→N または N→1 に組み立て、`WiringBuilder::build` した袋を `Machine` が持つ。`drive` は Sink コールバックをすぐ呼ぶ。GPIO enable やプル／衝突の合成は後続の回路部品。既存の `Machine::new` は空配線。
