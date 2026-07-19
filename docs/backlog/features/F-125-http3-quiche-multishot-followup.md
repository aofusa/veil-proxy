# F-125: HTTP/3 quiche / io_uring 受信の後続（F-124 引き継ぎ）

**ステータス: 未着手（引き継ぎ用・2026-07-19 時点の現状固定）**  
**優先度: P1**  
**親・前提: [F-124](F-124-http3-quiche-cc-multishot.md)（完了・部分達成）**  
**ブランチ参考: `feat/http3-quiche`（base 付近 `5b55ec8` から F-124 コミット群）**

---

## 0. このドキュメントの目的（別 AI エージェント向け）

このファイル **だけ** を読めば、次を把握して実装を継続できるようにする。

1. F-124 で **何が入り、何が意図どおりで、何が後退・未達か**
2. **変更前後**の HTTP/3（quiche）実装の差分
3. **主要ファイルと責務**
4. **残りタスク**（必須 / 推奨 / 任意）と受け入れ条件
5. **やってはいけないこと**（AGENTS.md ホットパス規則・feature 方針）
6. 検証・perf の既存結果と注意点

**ビルドやテストは本チケット作成時点では実行不要**（委譲先が実装時に実施する）。

プロジェクト共通ルールの正本はリポジトリ直下の [AGENTS.md](../../../AGENTS.md)。本チケットはそれに **反しない** こと。

---

## 1. 背景と依頼の元要件（ユーザー意図）

元依頼（要約）は次の 3 点＋運用要件だった。

| # | 要件 | F-124 での結果 |
|---|------|----------------|
| 1 | `quiche::Config` で設定できる **輻輳制御・Pacing 等** を `config.toml` の **`[http3]`** で設定可能にする | **達成** |
| 2 | quiche まわりバッファ（mmsg バッチ）の **既定を 64** にし、`[http3]` で変更可能にする | **達成** |
| 3 | 高レベル API 廃止 → **低レベル API** + ホットパス規則 + **`IORING_OP_RECVMSG_MULTISHOT` 活用** + seccomp 等の許可追記 | **部分達成**（下記「ギャップ」） |
| 付帯 | 適宜コミット、各種 build/test/E2E、clippy/fmt、README/config 更新、Docker `--net=host` で GSO/GRO on/off、tools/perf で HTTP/3 代表 1 本 | **おおむね実施**。tools/perf 公式イメージ再ビルドは未完 |

参考レポートの要点（幻覚注意）:

- Cloudflare `quiche` に「gcongestion / zero-copy」という **魔法の Cargo feature はない**（ただし enum に `Bbr2Gcongestion` 等は存在する）。
- 効くのは **低レベル send/recv + CC/Pacing チューニング + UDP GSO/GRO + mmsg バッチ拡大**。
- 真のゼロコピーは「暗号化後 UDP ペイロードをカーネルへどう渡すか」。

---

## 2. 変更前（F-124 直前）の実装概要

### 2.1 quiche

- もともと **sans-IO 低レベル API** を使用:
  - `quiche::Header::from_slice` / `quiche::accept` / `Connection::recv` / `Connection::send`
  - `h3::Connection`（HTTP/3 層）
- tokio/quiche 高レベル async ラッパへの依存は **もともと無い**。
- `quiche::Config` には idle timeout・初期ウィンドウ等は TOML から載せていたが、**CC / pacing / hystart は未配線**（実質 quiche 既定 CUBIC 等）。

### 2.2 設定配線のバグ

- `src/entry.rs` の HTTP/3 ワーカー起動が `Http3ServerConfig { cert_pem, key_pem, ..Default::default() }` 中心で、**`[http3]` の gso・ウィンドウ・timeout 等がワーカーに渡らない**ケースがあった。

### 2.3 UDP 受信

- 先頭: `recv_gro_async` = **io_uring `POLL_ADD`（readable）+ 同期 `recvmsg`（GRO cmsg）**
- 継続 drain: **同期 `recvmmsg`**（`MMSG_RECV_BATCH = 16` 固定）
- 送信: quiche `send` → GSO 連結 → **`sendmmsg`**（`MMSG_SEND_BATCH = 16` 固定）

### 2.4 関連完了チケット

- F-33 GSO/GRO、F-45/F-60 GRO、F-115 recvmmsg/sendmmsg、F-105 cert reload 等は前提として残っている。

---

## 3. F-124 で入った変更（現状・実装済み）

### 3.1 `[http3]` 新設定キー

| キー | 型 | デフォルト | quiche / 用途 |
|------|-----|------------|----------------|
| `cc_algorithm` | string | `"bbr"` | `set_cc_algorithm_name`（不明値は warn して bbr） |
| `pacing` | bool | `true` | `enable_pacing` |
| `max_pacing_rate` | optional u64 | なし | `set_max_pacing_rate`（バイト/秒） |
| `hystart` | bool | `true` | `enable_hystart` |
| `mmsg_batch_size` | usize | `64` | 1..=128 に clamp。recvmmsg/sendmmsg と RECVMSG 経路の batch 幅 |

**受け付ける CC 名（quiche 0.24 系）**: `reno` / `cubic` / `bbr` / `bbr2` / `bbr2_gcongestion`（大小は quiche の from_str に依存。実装は trim 後そのまま渡す）。

**コード**:

- `src/config.rs` — `Http3ConfigSection` + `to_http3_config()`
- `src/http3_server.rs` — `Http3ServerConfig` + `run_http3_server_async` で `quic_config` に適用
- `src/entry.rs` — `loaded_config.http3_config.to_http3_config(...)` を clone して各ワーカーへ

**ドキュメント**:

- `examples/config.toml` / `contrib/config/config.toml`（コメントにデフォルト記載）
- `README.md` / `docs/readme/README.ja.md` の設定表
- **`AGENTS.md` は未更新**（残りタスク）

### 3.2 mmsg バッチ既定 64・ランタイム可変

- `src/udp/socket.rs`:
  - `MMSG_BATCH_DEFAULT = 64` / `MMSG_BATCH_MAX = 128`
  - `MmsgRecvScratch::with_batch(n)` / `MmsgSendScratch::with_batch(n)`
  - 固定長配列 `[T; 16]` から **Vec + batch フィールド** へ
- `http3_server` は `config.mmsg_batch_size` で scratch 確保、送信閾値も `mmsg.batch_size()` 使用

### 3.3 受信経路（現行・本番既定）

Linux + `veil_rt_uring`（デフォルト io_uring バックエンド）:

1. **先頭 1 データグラム**: `src/runtime/uring/udp_recv.rs` の `MultishotUdpRecv`
   - 実体は **`IORING_OP_RECVMSG` 単発** + `IORING_RECVSEND_POLL_FIRST`
   - 従来 msghdr（peer sockaddr + GRO cmsg + payload iov）をワーカー起動時に 1 回配線し再利用
   - 名前は `MultishotUdpRecv` だが **カーネル `IORING_RECV_MULTISHOT` は立てていない**
2. **継続 drain**: 既存 `recv_mmsg_sync`（同期 recvmmsg、EAGAIN まで、上限 `H3_RECV_DRAIN_MAX`）
3. 無効化: 環境変数 `VEIL_H3_MULTISHOT=0`（false/off も可）→ 従来 `recv_gro_async` + recvmmsg
4. reactor ビルド（`--features epoll` 等）: `used_multishot = false` で従来経路のみ

送信は変更の中心ではなく、batch 幅の設定連動のみ。

### 3.4 ランタイム / セキュリティの追加（一部未使用）

| 場所 | 内容 | 現行ホットパスで使用? |
|------|------|----------------------|
| `ring.rs` | `IORING_RECV_MULTISHOT` / `IORING_RECVSEND_POLL_FIRST` / `IORING_CQE_BUFFER_SHIFT` 定数 | POLL_FIRST のみ使用 |
| `executor.rs` | `PROXY_ALLOWED_OPCODES` に `RECVMSG`, `PROVIDE_BUFFERS`, `REMOVE_BUFFERS` | RECVMSG のみ使用 |
| `executor.rs` | `MultishotActive` / `alloc_multishot_op` / `take_multishot_cqe` | **未使用**（真 multishot 試作の残骸） |
| `security.rs` | recvmmsg コメントに F-124 フォールバック言及 | syscall 自体は F-115 から許可済み |

### 3.5 関連コミット（参考）

`feat/http3-quiche` 上の F-124 系（新しい順の例）:

- docs: F-124 チケット同期 / ログ文言
- `fix(http3): RECVMSG 待機を安定な単発経路へ（recvmmsg drain 併用）`
- `feat(http3): io_uring RECVMSG … + mmsg バッチ既定 64`
- `feat(http3): quiche CC/pacing/hystart と mmsg バッチを [http3] 設定化`

---

## 4. ギャップ（当初要件 vs 現状）— 最重要

### 4.1 未達: 真の `IORING_OP_RECVMSG` Multishot

依頼文言は **`IORING_OP_RECVMSG_MULTISHOT` 活用**（実態は opcode `IORING_OP_RECVMSG` + ioprio の `IORING_RECV_MULTISHOT` + 通常は provided buffers / buffer ring）。

F-124 では一度 provided buffers + multishot を試みたが、

- **unconnected multi-peer UDP** で peer アドレスを per-CQE 安全に取るのが難しい（shared msg_name レース。multishot では `io_uring_recvmsg_out` 埋め込みレイアウトが必要）
- ENOBUFS / re-provide の運用がシビア
- E2E で HTTP/3 大量失敗

→ **単発 RECVMSG + recvmmsg drain に後退**（安定優先）。

したがって現状は:

- 「POLL_ADD 二重往復の排除」は達成
- 「カーネル Multishot 1 SQE で連続 CQE」は **未達成**

### 4.2 誤解しやすい点: quiche「高レベル API 廃止」

コードベースは **もともと低レベル**。F-124 で quiche の別 API ファミリーへ切り替えたわけではない。やったのは **Config チューニング + UDP I/O の io_uring 化**。

### 4.3 ドキュメント・命名の負債

- 型名 `MultishotUdpRecv` / env `VEIL_H3_MULTISHOT` / 一部ログ・コメントが **実装と不一致**
- `executor.rs` の `PROXY_ALLOWED_OPCODES` コメントが「MULTISHOT + provided buffers へ移行」と書いてあるが **現行ホットパスはそうではない**
- F-124 チケットのリスク欄に ENOBUFS / multishot 6.0+ など **撤回実装向け記述が残る**
- `backlog.md` の F-124 一行要約も multishot 完了のように読める（**過大**）

### 4.4 未完了の周辺

| 項目 | 状態 |
|------|------|
| AGENTS.md | **未更新** |
| tools/perf 公式（`veil:glibc` 再ビルド + `CONFIG_GLOB='h2_1_feat_http3'`） | Docker 再ビルドがキャンセルされ **未完** |
| host net 簡易ベンチ | 実施済み（下記 §7） |
| E2E | full で多数 pass。config reload / large body にフレーク疑い |

---

## 5. 主要ファイルマップ（委譲先が触る場所）

| パス | 役割 |
|------|------|
| `src/config.rs` | `[http3]` serde・default・`to_http3_config` |
| `src/entry.rs` | ワーカーへの `Http3ServerConfig` 配線 |
| `src/http3_server.rs` | quiche Config 適用、メインループ（RECV 分岐・send・h3） |
| `src/udp/socket.rs` | GSO/GRO、recvmmsg/sendmmsg、batch 定数と scratch |
| `src/runtime/uring/udp_recv.rs` | **現行** `IORING_OP_RECVMSG` 単発 Future |
| `src/runtime/uring/executor.rs` | 許可 opcode、**未使用** multishot OpTable |
| `src/runtime/uring/ring.rs` | opcode / RECV 修飾フラグ定数 |
| `src/security.rs` | seccomp 許可表・コメント |
| `examples/config.toml` / `contrib/config/config.toml` | 設定リファレンス |
| `README.md` / `docs/readme/README.ja.md` | ユーザー向け設定表 |
| `AGENTS.md` | **未更新** — エージェント向け正の制約 |
| `docs/backlog/features/F-124-...md` | F-124 本体（完了だが記述にズレ） |
| `tools/perf/` | 公式計測ハーネス |

ホットパス規則（要約）: データプレーンで同期 I/O・不必要 alloc 禁止。io_uring 新規 opcode 拡大は慎重に。詳細は AGENTS.md。

---

## 6. メインループの現行アルゴリズム（実装の正）

`run_http3_server_async`（`http3_server.rs`）概略:

```
mmsg_batch = clamp(config.mmsg_batch_size)
mmsg_scratch = MmsgRecvScratch::with_batch(mmsg_batch)
ms_recv = MultishotUdpRecv::new(fd, mmsg_batch)  // uring+linux, unless VEIL_H3_MULTISHOT=0

loop:
  shutdown / cert reload / min timeout
  if ms_recv is Some:   # Linux veil_rt_uring 既定
    select_biased:
      first = ms_recv.recv_one()   # IORING_OP_RECVMSG 単発
      notify.wait()
      sleep(timeout)
    on first Ok:
      process_datagram_segments(payload of first)  # quiche recv 等
      while drain < H3_RECV_DRAIN_MAX:
        n = socket.recv_mmsg_sync(mmsg_scratch)
        for each: process_datagram_segments(...)
        if n < batch: break
      send_pending_packets(...)
  else:
    # 従来: recv_gro_async + recvmmsg drain
  init_h3 / process_h3_events / drive_proxy_streams
  send_pending_packets(...)
  yield_now()
```

`process_datagram_segments`: ゼロコピーで `Header::from_slice` + `conn.recv`（F-45 prev_cid 最適化あり）。

---

## 7. 検証・perf メモ（既実施・条件付き）

### 7.1 実施済み（F-124 作業中）

- `cargo build`: default / no-default-features / http3 / full — warning なし方向
- clippy `-D warnings`（http3 / full）
- `cargo test --lib --features full`: 768 pass 程度
- E2E full: 529 pass / 2 fail のあと、large body は再実行 pass（フレーク疑い）
- Docker `--net=host` + h2load h3: **GSO on/off とも 2xx 成功**
- host 簡易ベンチ（release、GSO on、bbr、batch 64、h2load `-n30000 -c100 -m10`）:
  - 約 **5713 / 7711 / 7351 req/s**（median ~7351、non-2xx 0）
  - 比較用 `docs/perf` ブリッジ `h2_1_feat_http3` http3 は **~835 req/s** → **条件が違う**（host net vs docker bridge）。公平比較は tools/perf 再計測が必要

### 7.2 未実施・注意

- `docker build -t veil:glibc -f docker/Dockerfile.glibc .` は **キャンセル済み**。`veil:glibc` イメージは古いままの可能性大。
- tools/perf: `CONFIG_GLOB='h2_1_feat_http3' bash tools/perf/run_perf.sh` は **現行ソース入りイメージ**で行うこと。

---

## 8. 残りタスク（委譲先の ToDo）

方針は **どちらか一方を選んでから** 実装する（両方中途半端にしない）。

### 方針 A — 真の Multishot を完成させる（元依頼 3c の完全達成）

1. 設計: unconnected multi-peer UDP で peer を per-CQE 安全に取る方法を固定する。
   - 推奨候補: **provided buffers / buffer ring** + バッファ先頭の `io_uring_recvmsg_out` + name/control/payload レイアウト（kernel 6.0+、man `io_uring_prep_recvmsg_multishot` / `io_uring_recvmsg_out`）。
2. `udp_recv.rs` を真 multishot 実装に差し替え（または新モジュール）。
3. 既存の `alloc_multishot_op` / `take_multishot_cqe` / CQE `F_MORE` 処理を **実際に接続**。
4. バッファ再提供（`IORING_OP_PROVIDE_BUFFERS`）をホットパス規則に沿って（コールドで pool 確保、再提供は fire-and-forget またはバッチ、alloc 最小化）。
5. 失敗時フォールバック（単発 RECVMSG または POLL+recvmmsg）を維持。
6. E2E HTTP/3 全系 + Docker GSO on/off + host/bridge perf。
7. ドキュメント・命名を true multishot に合わせて統一。

**リスク**: 複雑度・kernel 差・ENOBUFS・security surface（PROVIDE が正当化される）。

### 方針 B — Multishot を公式に断念し、現状を「正」として綺麗にする（推奨しやすい）

1. **命名整理**: `MultishotUdpRecv` → 例 `UringRecvMsg` / `H3UdpRecv`。env も `VEIL_H3_URING_RECVMSG=0` 等へ（互換で旧名を読むなら移行期間を書く）。
2. **デッドコード削除 or 明示**:
   - 使わないなら `MultishotActive` / `alloc_multishot_op` / `take_multishot_cqe` を削除。
   - 使わないなら `PROVIDE_BUFFERS` / `REMOVE_BUFFERS` を `PROXY_ALLOWED_OPCODES` から外し security surface を戻す（残すなら「将来用・未使用」とコメントで正直に）。
3. **コメント修正**: `executor.rs` F-124 説明、F-124 チケット、`backlog.md` の F-124 一行、http3 ログ文言。
4. **AGENTS.md** に HTTP/3 受信経路・`[http3]` 新キー・batch 既定 64 を追記。
5. F-124 を「部分完了・受信は RECVMSG 単発+recvmmsg」と履歴として正確化（本 F-125 が後続）。

### 方針 A/B 共通の推奨タスク

| ID | タスク | 優先 |
|----|--------|------|
| T1 | AGENTS.md 更新 | 高 |
| T2 | F-124 / backlog 一行 / executor コメントの事実同期 | 高 |
| T3 | tools/perf: 現行ソースで `veil:glibc`（必要なら musl）再ビルド → `CONFIG_GLOB='h2_1_feat_http3'`（ITERATIONS は環境に応じ 1〜3）→ `docs/perf` に結果メモ | 高（依頼にあった） |
| T4 | E2E フレーク切り分け（`test_config_reload_adds_route_via_sighup` / large body） | 中 |
| T5 | 単体テスト: RECVMSG 経路の配線・batch clamp・CC 名フォールバック | 中 |
| T6 | reactor（epoll）ビルドで HTTP/3 が従来経路で通ること確認 | 中 |

### 受け入れ条件（F-125 完了の定義）

**方針 B を採る場合**（最小完了）:

- [ ] 命名・コメント・AGENTS・F-124 記述が実装と一致
- [ ] 未使用 multishot OpTable / 不要 opcode 許可の整理が完了（削除 or 正当な理由付き保持）
- [ ] `cargo build`（full / default / no-default-features / 関連 features）warning なし
- [ ] clippy / fmt クリーン（不当な `allow(dead_code)` 禁止）
- [ ] E2E（少なくとも HTTP/3 関連）が安定して通る
- [ ] tools/perf 代表 1 本の結果を docs（perf または artifacts）に記録し、旧 ~835 との **同一条件**比較メモがある

**方針 A を採る場合**（完全完了）:

- 上記に加え:
- [ ] ホットパスで **実際に** `IORING_RECV_MULTISHOT`（+ provided buffers または buffer ring）が動作
- [ ] multi-peer で peer アドレス誤りがない（E2E / 専用テスト）
- [ ] ENOBUFS 時にイベントループをブロックせず回復
- [ ] Docker GSO on/off・seccomp プロファイルで破綻しない

---

## 9. 実装時の注意（AGENTS.md 要約）

1. **ホットパス**: 同期 `std::net` / ブロッキング read・write・sleep 禁止。alloc を増やさない。ゼロコピー維持（`Bytes`、scratch 再利用、slice 直渡し）。
2. **ランタイム**: データプレーンは tokio 非依存。io_uring は `src/runtime/uring/`。reactor 追加でも uring の等価性を壊さない。
3. **feature**: `default = []` 相当方針を崩さない。http3 無効でもコンパイル可能。
4. **設定変更**: README（.ja）と `examples/config.toml` / `contrib/config/config.toml` を **同じ PR** で更新。デフォルト値は config コメントに明記。
5. **バックログ**: features 個別 md を変えたら **必ず** [backlog.md](../backlog.md) も同じ変更で更新。
6. **成果物**: AI 調査メモは `docs/artifacts/` のみ。
7. **コミット**: 1 目的 1 コミット粒度。force-push や無関係リファクタ禁止。

---

## 10. 推奨作業手順（委譲先）

1. 本ファイル §8 で **方針 A or B** を決める（ユーザー指示があればそれに従う。無ければ **方針 B を先に綺麗にしてから A を別チケット**でも可）。
2. 現状ブランチ `feat/http3-quiche` または main 取り込み後の差分を `git log` / 本ファイル §3 と照合。
3. 実装 → `cargo fmt` / clippy / feature マトリクス build → `cargo test` / `./tests/e2e_setup.sh test`。
4. Docker `--net=host` で GSO on/off。
5. tools/perf HTTP/3 代表 1 本（イメージは現行ソース）。
6. AGENTS / README / config / backlog を同期。
7. F-125 の受け入れチェックリストを埋め、`backlog.md` の対応状況を **完了** に。

---

## 11. クイック参照: 設定デフォルト（実装の正）

```toml
[http3]
# cc_algorithm = "bbr"
# pacing = true
# max_pacing_rate = ...   # 省略時は制限なし
# hystart = true
# mmsg_batch_size = 64    # 1..=128
# gso_gro_enabled = false # 既存。perf では true 推奨の環境あり
```

環境変数:

- `VEIL_H3_MULTISHOT=0` — RECVMSG 経路を切り、POLL+`recv_gro_async` フォールバック（名前は歴史的経緯）。

---

## 12. 関連リンク

- [F-124](F-124-http3-quiche-cc-multishot.md) — 本変更の本体（完了・記述ズレあり）
- [F-115](F-115-http3-recvmmsg-sendmmsg-batching.md) — recvmmsg/sendmmsg
- [F-33](F-33-http3-gso-gro-offload.md) — GSO/GRO
- [F-111](F-111-perf-regression-check-http3-gso-zerocopy.md) — HTTP/3 perf 構造要因
- [docs/perf/README.md](../../perf/README.md) — 計測履歴（旧 http3 ~835 req/s 等）
- [AGENTS.md](../../../AGENTS.md)

---

## 13. 変更履歴（本チケット）

| 日付 | 内容 |
|------|------|
| 2026-07-19 | F-124 実装レビューに基づき引き継ぎドキュメントとして新設。現状・ギャップ・残りタスクを固定。 |
