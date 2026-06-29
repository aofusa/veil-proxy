# F-30: L4 ストリームプロキシの `splice(2)` ゼロコピー転送

## 出典

`docs/artifacts/architecture_analysis_v2.md` / `v4.md` / `v5.md`（L4 プロキシの究極のゼロコピー化）。

## 概要

L4 TCP プロキシのバイダイレクショナル転送を、ユーザースペースバッファを介さず
io_uring の `IORING_OP_SPLICE` でカーネル内完結させ、メモリコピーとアロケーションを
完全にゼロにする。

## 現状の問題

`src/l4/proxy.rs` の `forward_direction` は、コネクションごとに 64KB の `Vec<u8>`
（`BUF_SIZE`）を確保し、`read()`→`write()` でユーザースペースを経由してリレーしている。

- 1 万接続で各方向 64KB ＝ 約 1.28GB のヒープバッファ。
- カーネル ↔ ユーザースペース間のコピーが read/write ごとに発生（ゼロコピーでない）。

> 注: 現状でもバッファは「コネクションあたり 1 回」確保し ownership handoff で再利用しているため
> ループ内の再確保は無い。本チケットはさらに踏み込み、ユーザースペースバッファ自体を撤廃する。

## 改修内容

1. `src/runtime/` に `pipe(2)` ラッパーと `splice` Future を追加する。
   - `IORING_OP_SPLICE` の SQE フィールド: `fd_in`(`splice_fd_in`), `off_in`,
     `fd`(=out), `off_out`, `len`, `splice_flags`(`SPLICE_F_MOVE | SPLICE_F_NONBLOCK`)。
   - SPLICE は fd→fd 直送ができないため、`src_fd → pipe → dst_fd` の 2 段 splice にする。
     pipe バッファ（デフォルト 64KB、`fcntl(F_SETPIPE_SZ)` で拡張可）はカーネル内ページ参照
     のため、ユーザースペースへのコピーが発生しない。
2. `forward_direction` を splice ベースに置換する。コネクションあたり pipe を 1 本確保
   （fd 2 本）、`Vec<u8>` は撤廃。
3. TLS passthrough / `tls = "none"` の素の TCP ストリームでのみ splice 経路を使う
   （termination 経路は対象外）。

## 受け入れ条件

- [ ] L4 E2E（`l4-proxy` feature）が回帰なし。
- [ ] `forward_direction` のホットパスに `Vec`/`BytesMut` のユーザースペースバッファが無い。
- [ ] 大容量転送でユーザースペース RSS が接続数に比例して増えない。

## 依存・リスク

- `IORING_OP_SPLICE` は `PROXY_ALLOWED_OPCODES`（`src/runtime/executor.rs`）に既に許可登録済み。
- pipe fd の Drop/クローズ管理、short-splice 時の継続ループ、EOF 検出に注意。
- splice が EINVAL を返す環境（特殊 fd）向けに、ユーザースペース read/write へのフォールバックを残す。

## 対応状況: 完了

### 事前準備（E2E + 既存バグ修正）

L4 には E2E が無く検証できなかったため、まず E2E を追加（`tests/e2e_setup.sh` に `[[l4]]` TLS
パススルーリスナー、`tests/e2e_tests.rs` に `test_l4_tcp_passthrough_forward` /
`test_l4_passthrough_large_payload`）。これにより [[B-09]]（forward が読み取り n でなくバッファ
全長 64KB を送信し転送破損）を発見・修正した。

### 実装

- `src/runtime/splice.rs`（新規）: `Pipe`（`pipe2(O_NONBLOCK|O_CLOEXEC)`、Drop で両端クローズ）と
  `SpliceFuture`（`IORING_OP_SPLICE`、`off_in/off_out=-1`、`SPLICE_F_MOVE|NONBLOCK|MORE`、B-07 流の
  in-flight Drop detach）を追加。`runtime/mod.rs` に `pub mod splice`。
- `src/l4/proxy.rs`: `forward_direction_splice` を追加し、`bidirectional_forward` を splice ベースへ
  変更。各方向に pipe を 1 本割り当て `src(socket) → pipe → dst(socket)` の 2 段 splice で
  **カーネル内ゼロコピー転送**（ユーザースペースバッファの確保・コピーが一切なし）。
  `readable()`/`writable()`（POLL_ADD）で待機しノンブロッキング splice をドレインループで回す
  （アイドル接続は io_uring ワーカーを占有しない）。dst 満杯時は `writable().await` で
  バックプレッシャ対応。
- pipe 作成失敗時（fd 上限等）はユーザースペース read/write（B-09 修正済み）へ安全フォールバック。
- io_uring 新規オペコードの追加なし（SPLICE は既に許可済み）= セキュリティサーフェス不変。

### 検証

- 新規 L4 E2E（基本転送 + 10KB ペイロード整合性）が splice 経路で通過。
- 大容量転送でユーザースペースバッファを確保しない（各方向 pipe 1 本のみ、64KB のヒープ
  確保が消滅）。
