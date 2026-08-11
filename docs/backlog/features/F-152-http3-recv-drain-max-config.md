# F-152: reactor 経路の UDP drain 上限を `[http3] recv_drain_max` で設定可能にする

- 優先度: P2
- 状態: 完了
- 関連: F-151（HTTP/3 メインループのイベント駆動化）、F-115、F-130

## 背景

F-151 の事前実験（[`docs/artifacts/f151_h3_loop_scaling_experiment.md`](../../artifacts/f151_h3_loop_scaling_experiment.md)）で、
**「1 イテレーションで扱うデータグラム数」だけを変えるとスループットが 3.2 倍動く**
ことが判明した（`mmsg_batch_size` を 8 → 128、128 接続・小レスポンス）。
トラフィック量・暗号処理量・輻輳制御は同一で、変わったのはループ回数だけである。

つまりこの量は **HTTP/3 の性能を左右する最重要チューニングパラメータ**だが、
バックエンドによって「何がその量を決めるか」が異なっていた。

| バックエンド | 1 イテレーションのデータグラム数を決めるもの | 設定可否 |
|---|---|---|
| Linux io_uring（既定） | `[http3] mmsg_batch_size` 本の `IORING_OP_RECVMSG` を常時 in-flight（F-130） | **設定可**（1..=128） |
| readiness reactor（**FreeBSD** / OpenBSD / NetBSD / macOS、Linux `--features epoll`） | `recv_mmsg_sync` を繰り返す drain ループの上限 `H3_RECV_DRAIN_MAX` | **ソース定数（64）で固定** |

**FreeBSD 等の reactor バックエンドだけが、この最重要パラメータを設定できない**状態だった。
`mmsg_batch_size` を大きくしても、reactor 経路では `H3_RECV_DRAIN_MAX = 64` が
1 イテレーションの合計データグラム数を 64 で頭打ちにしてしまう。

## 改修内容

`src/http3_server.rs` のハードコード定数

```rust
const H3_RECV_DRAIN_MAX: usize = 64;
```

を **設定キー `[http3] recv_drain_max`** に置き換える（`mmsg_batch_size` と同じ作法）。

- 既定値 `64`（`H3_RECV_DRAIN_MAX_DEFAULT`）＝ **従来の定数と同値**。
  設定を書かなければ挙動は一切変わらない。
- クランプ範囲 `1..=4096`（`H3_RECV_DRAIN_MAX_LIMIT`）。`mmsg_batch_size` が
  `MMSG_BATCH_MAX = 128` でクランプされるのと同じ形。
  `0` は無限ループを避けるため `1` へ引き上げる。
- **io_uring 経路（Linux 既定）は本設定を参照しない**。あちらは `mmsg_batch_size` 本の
  `IORING_OP_RECVMSG` パイプライン（F-130）で同じ役割を果たしているため。
  非対象バックエンドで指定しても受理して無視する（既存の非対象 OS 向け設定キーと同じ方針）。
- 起動時ログの `[HTTP/3] quiche transport: ...` 行に `recv_drain_max=` を追加。

### クランプ上限を 4096 にした理由

受信バッファは `mmsg_batch_size` 個ぶんのスクラッチ（`MmsgRecvScratch`）を
**使い回すだけ**なので、本値を大きくしてもメモリ使用量は増えない（drain ループの
反復回数が増えるだけ）。一方で drain 中は送信・タイムアウト処理・バックエンド通知が
待たされるため、**レイテンシ（特に p99）とのトレードオフ**になる。
青天井にすると 1 接続のバーストで他接続の応答が止まりうるため上限を設けた。

## 運用指針

- **スループット重視**（大量の小リクエスト・高接続数）: `256`〜`1024`
- **レイテンシ重視**: 既定の `64` 付近
- Linux 既定ビルド（io_uring）では効かない。同じ効果を得るには `mmsg_batch_size` を使う。

## テスト

- 単体（`src/http3_server.rs`）: 既定値が従来の定数と同値であること（＝設定を書かなければ
  挙動不変であることの担保）、クランプが `0 → 1` / 上限超過 → `4096` / 範囲内はそのまま、
  であること。`Http3ServerConfig::default()` にも反映されていること。
- ビルド: `--features full` / `--features "full,epoll"`（**reactor 経路が実際に
  コンパイルされる構成**）/ `--no-default-features` で warning 0。

## 検証結果

- `cargo test --lib --features full`: **854 passed / 0 failed**（F-152 の単体テストを追加）
- `cargo build --features full` / `--features "full,epoll"`（**reactor 経路が実際に
  コンパイルされる構成**）/ `--no-default-features` /
  `cargo clippy --features full --all-targets`: いずれも **warning 0**

### 実バイナリでの配線確認（設定 → サーバまで通っていることの実証）

`src/config.rs` には `deny_unknown_fields` が無く、**キー名を間違えても黙って無視される**
ため、静的なコードリーディングだけでは「設定が効いている」ことの証明にならない。
そこで実バイナリを起動し、起動ログ（`[HTTP/3] quiche transport: ...`）に出る値で
TOML → `Http3ConfigSection` → `Http3ServerConfig` → メインループの配線を実証した。

| `[http3] recv_drain_max` の指定 | 起動ログの値 | 期待 |
|---|---|---|
| （未指定） | `recv_drain_max=64` | 既定値 = 従来の定数と同値 ✓ |
| `0` | `recv_drain_max=1` | 下限クランプ（無限ループ防止）✓ |
| `1024` | `recv_drain_max=1024` | 範囲内はそのまま ✓ |
| `99999` | `recv_drain_max=4096` | 上限クランプ ✓ |

**この検証で実際に不具合を 1 件検出した**: 最初の実行では起動ログが旧書式のままで
`recv_drain_max=` が出なかった。原因は `cargo build`（debug）しか実行しておらず
`target/release/veil` が古いままだったこと。**「設定が効くか」はリリースバイナリを
建て直して実際に起動して確かめること。**
