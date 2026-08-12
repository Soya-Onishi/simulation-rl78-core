# simulation-rl78-core

RL78 シミュレーション基盤（[issue #1](https://github.com/Soya-Onishi/simulation-rl78-core/issues/1)）。
TCG コアは [tlib](https://github.com/Soya-Onishi/tlib)（`rl78` ブランチ）、その上位を Rust で組み立てる。

設定ファイルは使わない。マシン記述はすべて Rust コード。

## クレート

| クレート | 役割 |
|----------|------|
| `sim-kernel` | 仮想時計・イベントキュー・`MemoryBus` / `MemoryMapped`・start/stop/quit・検査 API |
| `rl78-core` | RL78 コア（tlib 静的リンク、CPU ラッパ、最小メモリマップ、Magic probe、ELF 口） |
| `simulation-rl78-core` | 薄い CLI（bin のみ）。シミュレーション本体は別スレッド |

## 開発順（issue #1）

| 段階 | 内容 | 状態 |
|------|------|------|
| **A** | Workspace / クレート骨格 | 完了 |
| **B** | sim-kernel 実行核 | 完了 |
| **C** | tlib submodule + cmake + 最小 FFI | 本ブランチ |
| **D** | tlib メモリ CB → Bus、ROM/RAM、未マップ | 未着手 |
| **E** | ELF + Magic probe 接続 + `minimal_machine` 本実装 | 口だけ先行 |
| **F** | CLI `start` / `stop` / `quit` | チャネル先行（実機接続は E） |
| **G** | 最小ゲスト ELF で Magic 出力スモーク | 未着手 |

## ビルド / テスト

初回は tlib submodule が必要:

```bash
git submodule update --init --recursive
cargo test
cargo run
```

`rl78-core` の `build.rs` が [tlib](https://github.com/Soya-Onishi/tlib)（`rl78` ブランチ）を CMake でビルドし、`libtlib.a` を静的リンクする。要: `cmake`、C コンパイラ、`pthread`。

CLI は標準入力から `start` / `stop` / `quit` を受け付ける。`Rl78Cpu` は tlib 上で動くが、バス接続（段階 D）と ELF（段階 E）前のため、ゲスト Magic 出力はまだ出ない。
