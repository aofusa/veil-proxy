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
