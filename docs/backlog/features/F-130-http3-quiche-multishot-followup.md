# F-130: HTTP/3 quiche / io_uring 受信の後続 + UDP/HTTP3 極限 io_uring 化（F-129 引き継ぎ）

**ステータス: 未着手（引き継ぎ用・2026-07-19 時点の現状固定、§14 は同日追記）**  
**優先度: P1**  
**親・前提: [F-129](F-129-http3-quiche-cc-multishot.md)（完了・部分達成）**  
**ブランチ参考: `feat/http3-quiche`（base 付近 `5b55ec8` から F-129 コミット群）**  
**ユーザー追加指示（2026-07-19）: UDP / HTTP/3 データプレーンを可能な限り io_uring 化し、極限パフォーマンスを突き詰める。調査と実装を本チケットの委譲範囲に含める。**

---

## 0. このドキュメントの目的（別 AI エージェント向け）

このファイル **だけ** を読めば、次を把握して実装を継続できるようにする。

1. F-129 で **何が入り、何が意図どおりで、何が後退・未達か**
2. **変更前後**の HTTP/3（quiche）実装の差分
3. **主要ファイルと責務**
4. **残りタスク**（必須 / 推奨 / 任意）と受け入れ条件
5. **やってはいけないこと**（AGENTS.md ホットパス規則・feature 方針）
6. 検証・perf の既存結果と注意点
7. **UDP / HTTP/3 の io_uring 化は可能か**、現状どこまで io_uring か、**極限性能のための設計・実装フェーズ**（§14）

**ビルドやテストは本チケット作成時点では実行不要**（委譲先が実装時に実施する）。

プロジェクト共通ルールの正本はリポジトリ直下の [AGENTS.md](../../../AGENTS.md)。本チケットはそれに **反しない** こと。

---

## 1. 背景と依頼の元要件（ユーザー意図）

元依頼（要約）は次の 3 点＋運用要件だった。

| # | 要件 | F-129 での結果 |
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

## 2. 変更前（F-129 直前）の実装概要

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

## 3. F-129 で入った変更（現状・実装済み）

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
| `security.rs` | recvmmsg コメントに F-129 フォールバック言及 | syscall 自体は F-115 から許可済み |

### 3.5 関連コミット（参考）

`feat/http3-quiche` 上の F-129 系（新しい順の例）:

- docs: F-129 チケット同期 / ログ文言
- `fix(http3): RECVMSG 待機を安定な単発経路へ（recvmmsg drain 併用）`
- `feat(http3): io_uring RECVMSG … + mmsg バッチ既定 64`
- `feat(http3): quiche CC/pacing/hystart と mmsg バッチを [http3] 設定化`

---

## 4. ギャップ（当初要件 vs 現状）— 最重要

### 4.1 未達: 真の `IORING_OP_RECVMSG` Multishot

依頼文言は **`IORING_OP_RECVMSG_MULTISHOT` 活用**（実態は opcode `IORING_OP_RECVMSG` + ioprio の `IORING_RECV_MULTISHOT` + 通常は provided buffers / buffer ring）。

F-129 では一度 provided buffers + multishot を試みたが、

- **unconnected multi-peer UDP** で peer アドレスを per-CQE 安全に取るのが難しい（shared msg_name レース。multishot では `io_uring_recvmsg_out` 埋め込みレイアウトが必要）
- ENOBUFS / re-provide の運用がシビア
- E2E で HTTP/3 大量失敗

→ **単発 RECVMSG + recvmmsg drain に後退**（安定優先）。

したがって現状は:

- 「POLL_ADD 二重往復の排除」は達成
- 「カーネル Multishot 1 SQE で連続 CQE」は **未達成**

### 4.2 誤解しやすい点: quiche「高レベル API 廃止」

コードベースは **もともと低レベル**。F-129 で quiche の別 API ファミリーへ切り替えたわけではない。やったのは **Config チューニング + UDP I/O の io_uring 化**。

### 4.3 ドキュメント・命名の負債

- 型名 `MultishotUdpRecv` / env `VEIL_H3_MULTISHOT` / 一部ログ・コメントが **実装と不一致**
- `executor.rs` の `PROXY_ALLOWED_OPCODES` コメントが「MULTISHOT + provided buffers へ移行」と書いてあるが **現行ホットパスはそうではない**
- F-129 チケットのリスク欄に ENOBUFS / multishot 6.0+ など **撤回実装向け記述が残る**
- `backlog.md` の F-129 一行要約も multishot 完了のように読める（**過大**）

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
| `docs/backlog/features/F-129-...md` | F-129 本体（完了だが記述にズレ） |
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

### 7.1 実施済み（F-129 作業中）

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
3. **コメント修正**: `executor.rs` F-129 説明、F-129 チケット、`backlog.md` の F-129 一行、http3 ログ文言。
4. **AGENTS.md** に HTTP/3 受信経路・`[http3]` 新キー・batch 既定 64 を追記。
5. F-129 を「部分完了・受信は RECVMSG 単発+recvmmsg」と履歴として正確化（本 F-130 が後続）。

### 方針 C — UDP / HTTP/3 データプレーンの極限 io_uring 化（**ユーザー明示の追加要求・最優先で調査・実装**）

詳細は **§14**。要約:

- **可能である**（受信・送信の completion 型 I/O、真 Multishot、送信 SENDMSG、固定バッファ等）。
- quiche 本体の暗号化・ステートマシンは **ユーザー空間 CPU** のまま（「HTTP/3 処理全体をカーネルに載せる」は不可能）。極限性能は **syscall/CQE 往復とコピーの最小化 + CC/GSO** で詰める。
- 方針 A は方針 C の **受信 half** と強く重複する。**方針 C を採る場合は A を内包**し、B の命名整理は C の完了後に行うか、C 実装と同時に命名を正す。
- 実装は **フェーズ分割 + 各フェーズで perf A/B**（退行したら戻す）。AGENTS ホットパス規則を厳守。

### 方針 A/B/C 共通の推奨タスク

| ID | タスク | 優先 |
|----|--------|------|
| T0 | **§14 の調査メモを実装前に短く確定**（採用する I/O 形態・kernel 前提・フォールバック） | 最高（ユーザー要求） |
| T1 | AGENTS.md 更新（HTTP/3 受信経路・設定・io_uring 方針） | 高 |
| T2 | F-129 / backlog 一行 / executor コメントの事実同期 | 高 |
| T3 | tools/perf: 現行ソースで `veil:glibc`（必要なら musl）再ビルド → `CONFIG_GLOB='h2_1_feat_http3'`（ITERATIONS は環境に応じ 1〜3）→ `docs/perf` に結果メモ | 高（依頼にあった） |
| T4 | E2E フレーク切り分け（`test_config_reload_adds_route_via_sighup` / large body） | 中 |
| T5 | 単体テスト: RECVMSG 経路の配線・batch clamp・CC 名フォールバック | 中 |
| T6 | reactor（epoll）ビルドで HTTP/3 が従来経路で通ること確認 | 中 |
| T7 | **§14 フェーズ実装**（受信フル io_uring → 送信 io_uring → multishot/buffer ring → 固定バッファ等） | 最高（ユーザー要求） |

### 受け入れ条件（F-130 完了の定義）

**方針 B のみ**を採る場合は最小完了だが、**ユーザーは方針 C（極限 io_uring 化）を明示要求している**ため、委譲先のデフォルト完了条件は **方針 C のフェーズ 1 以上 + ドキュメント整合** とする。B 単独完了はユーザーが「整理だけ」と指示した場合に限る。

**方針 B を採る場合**（最小完了・整理のみ）:

- [ ] 命名・コメント・AGENTS・F-129 記述が実装と一致
- [ ] 未使用 multishot OpTable / 不要 opcode 許可の整理が完了（削除 or 正当な理由付き保持）
- [ ] `cargo build`（full / default / no-default-features / 関連 features）warning なし
- [ ] clippy / fmt クリーン（不当な `allow(dead_code)` 禁止）
- [ ] E2E（少なくとも HTTP/3 関連）が安定して通る
- [ ] tools/perf 代表 1 本の結果を docs（perf または artifacts）に記録し、旧 ~835 との **同一条件**比較メモがある

**方針 A を採る場合**（真 Multishot のみ）:

- 上記に加え:
- [ ] ホットパスで **実際に** `IORING_RECV_MULTISHOT`（+ provided buffers または buffer ring）が動作
- [ ] multi-peer で peer アドレス誤りがない（E2E / 専用テスト）
- [ ] ENOBUFS 時にイベントループをブロックせず回復
- [ ] Docker GSO on/off・seccomp プロファイルで破綻しない

**方針 C を採る場合**（ユーザー要求のデフォルト・極限 io_uring 化）— §14 参照:

- [ ] §14.5 フェーズ 0 調査結果をチケット or `docs/artifacts/` に短く記録
- [ ] **受信**: ホットパスで libc `recvmmsg` / `POLL_ADD`+同期 recv に依存しない（真 Multishot または pipelined `IORING_OP_RECVMSG` 複数 in-flight）。EAGAIN 待ち専用の POLL は「フォールバック」に限定
- [ ] **送信**: ホットパスで libc `sendmmsg` に依存しない（`IORING_OP_SENDMSG` + GSO cmsg、必要なら複数 SQE）。writable 待ちは POLL または SENDMSG の非同期完了で扱う
- [ ] quiche は引き続き sans-IO（`recv`/`send`）。暗号化をカーネルに移す幻覚はしない
- [ ] `PROXY_ALLOWED_OPCODES` / seccomp / docker seccomp を必要 opcode に合わせて更新
- [ ] 各フェーズで host または tools/perf の **同一条件 A/B**（変更前 vs 後）を記録。退行時は原因切り分け
- [ ] AGENTS.md / README / config コメントを更新
- [ ] full features ビルド・HTTP/3 E2E・Docker `--net=host` GSO on/off

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

1. **ユーザーは方針 C（極限 io_uring 化）を明示要求している。** 特に指示がなければ **§14 に従い方針 C を主軸**とし、方針 A は C の受信フェーズとして内包する。方針 B（整理のみ）はユーザーが明示した場合のみ単独完了とする。
2. §14.3 **フェーズ 0**（調査・設計の短文固定）を先に書く（`docs/artifacts/` 可）。
3. 現状ブランチ `feat/http3-quiche` または main 取り込み後の差分を `git log` / 本ファイル §3 と照合。
4. §14 の **フェーズ 1 → 2 → …** を順に実装。各フェーズで fmt / clippy / HTTP/3 関連 test・E2E の一部、perf A/B。
5. Docker `--net=host` で GSO on/off。
6. tools/perf HTTP/3 代表 1 本（イメージは現行ソース）。
7. AGENTS / README / config / backlog を同期。命名・デッドコードは C 実装に合わせて整理（B の内容を吸収）。
8. F-130 の受け入れチェックリストを埋め、`backlog.md` の対応状況を **完了** に。

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

- [F-129](F-129-http3-quiche-cc-multishot.md) — 本変更の本体（完了・記述ズレあり）
- [F-115](F-115-http3-recvmmsg-sendmmsg-batching.md) — recvmmsg/sendmmsg
- [F-33](F-33-http3-gso-gro-offload.md) — GSO/GRO
- [F-111](F-111-perf-regression-check-http3-gso-zerocopy.md) — HTTP/3 perf 構造要因
- [docs/perf/README.md](../../perf/README.md) — 計測履歴（旧 http3 ~835 req/s 等）
- [AGENTS.md](../../../AGENTS.md)

---

## 14. UDP / HTTP/3 の io_uring 化は可能か・極限性能の突き詰め（委譲必須）

### 14.1 ユーザーへの回答（要約）

| 問い | 答え |
|------|------|
| UDP を io_uring 化できるか | **可能**。既にイベントループと「先頭 RECVMSG」はある。**drain の recvmmsg / 送信の sendmmsg を completion 型に載せる**のが次の本丸。 |
| HTTP/3「処理」全体を io_uring 化できるか | **I/O 境界のみ可能**。quiche の TLS/QUIC 暗号化・ACK・フロー制御・h3 フレーム処理は **ユーザー空間 CPU**。カーネルに HTTP/3 エンジンを載せる魔法はない。 |
| 以前は io_uring を使っていなかったのか | **使っていた**。ただし UDP は主に **`IORING_OP_POLL_ADD`（readiness）+ 同期 libc mmsg**。TCP ほど `IORING_OP_RECV/SEND` 直叩きではなかった。詳細は下記 §14.2。 |
| 委譲先がすべきこと | **調査（フェーズ 0）のうえ、方針 C で段階実装し perf で極限を詰める。** 本 §14 が指示書。 |

### 14.2 現状の「どこまで io_uring か」（正確な層分け）

```
【以前〜現在共通】
  ワーカー = 独自 io_uring ring + executor（TCP は RECV/SEND/ACCEPT 等）

【UDP 受信・以前】
  wait:  IORING_OP_POLL_ADD (readable)
  data:  libc recvmsg / recvmmsg（同期・非ブロッキング）

【UDP 受信・F-129 後（既定）】
  先頭 wait+data: IORING_OP_RECVMSG + POLL_FIRST（1 SQE）
  drain data:     libc recvmmsg（同期）← まだここが syscall 往復の塊
  フォールバック: POLL_ADD + recv_gro_async

【UDP 送信・ほぼ未変更】
  data:  libc sendmsg/sendmmsg + GSO cmsg
  wait:  EAGAIN 時 IORING_OP_POLL_ADD (writable)

【HTTP/3 ロジック】
  quiche::Connection::recv/send, h3::poll, ルーティング, バックエンド TCP…
  → すべてユーザー空間。io_uring 化の対象は「UDP/TCP の待ちとデータ移動」
```

**極限性能のボトルネック候補（優先度の目安）**

1. **per-datagram / per-batch の syscall 往復**（drain の recvmmsg、送信 sendmmsg、POLL との二重）
2. **select_biased 負け arm の timer arm/cancel**（F-115 で一部償却済み）
3. **quiche の per-packet 暗号化・状態更新（CPU）** — I/O 化では消えない
4. **コピー**（GSO 連結バッファ、GRO 分解、提供バッファと quiche 間）
5. **Docker veth で GSO/GRO 無効** — 計測は host net と bridge を分ける

F-111/F-115 の知見: HTTP/3 は CPU 余地を残して **syscall 律速**になりやすい → **I/O 経路の io_uring 化の ROI は高い**。

### 14.3 可能な io_uring 化メニューと難易度

| # | 施策 | 内容 | 期待効果 | 難易度 | 備考 |
|---|------|------|----------|--------|------|
| C0 | 調査・設計固定 | kernel 機能、ABI、フォールバック、測定計画 | 手戻り防止 | 低 | **最初に必須** |
| C1 | 受信 drain の io_uring 化 | 先頭以外も `IORING_OP_RECVMSG`（複数 in-flight または連続 submit）に。libc `recvmmsg` をホットパスから外す | 高 | 中 | multi-peer は **msghdr をスロットごと**に持つ |
| C2 | 真 Multishot 受信 | `IORING_RECV_MULTISHOT` + provided buffers / **buffer ring** + `io_uring_recvmsg_out` | 最高（理想） | **高** | 方針 A と同一。kernel 6.0+ |
| C3 | 送信の io_uring 化 | `IORING_OP_SENDMSG`（GSO: `UDP_SEGMENT` cmsg を msghdr に付与）。複数宛先は複数 SQE を 1 `io_uring_enter` で submit | 高 | 中 | 既存 TCP SENDMSG 実装（F-59）を参考 |
| C4 | 送信 Multishot / バッチ | 可能な範囲で linked SQE や複数 SENDMSG を一括 submit。`sendmmsg` 相当を ring 上で再現 | 中〜高 | 中 | セマンティクス（部分送信）に注意 |
| C5 | fixed buffers / registered buffers | `IORING_REGISTER_BUFFERS` または buffer ring で pin。CQE から bid で参照 | 中（コピー・ピン削減） | 中〜高 | セキュリティ・制限との兼ね合い |
| C6 | メインループ再設計 | notify/timeout と UDP CQE を **単一 poll 完了経路**に寄せ、select_biased の無駄 cancel を減らす | 中 | 中 | 既存 F-32 actor モデルを壊さない |
| C7 | quiche 側 | 既に sans-IO。CC=BBR・大きな window・pacing は F-129 済み。追加は計測駆動のパラメータ | 中（ワークロード依存） | 低 | 「quiche を io_uring 化」は誤解 |

**やっても無駄・幻覚になりやすいもの**

- quiche の暗号化をゼロにする「kernel HTTP/3」
- Cargo feature `zero-copy` / `gcongestion` という架空の銀弾（`Bbr2Gcongestion` は enum として存在し得るが feature 魔法ではない）
- Multishot を shared `msg_name` だけで multi-peer に使う（アドレス破壊）

### 14.4 推奨アーキテクチャ（方針 C の到達像）

```
                    io_uring SQ/CQ
                         │
     ┌───────────────────┼───────────────────┐
     │                   │                   │
 RECVMSG×N or        SENDMSG×M           TIMEOUT/POLL
 Multishot+pbuf      (+UDP_SEGMENT)      (timer/notify 補助)
     │                   │
     ▼                   ▲
  provided/fixed      GSO 連結済み
  buffers             batch (thread-local)
     │                   │
     └────► quiche recv/send (userspace) ◄───┘
                 │
            h3 / proxy / file …
```

**不変条件**

- quiche の `Connection` / `h3::Connection` は **単一スレッド・非 Send**（既存 actor モデル維持）
- ホットパスで **ブロッキング syscall 待ちをしない**（io_uring 完了待ちのみ）
- バッファはワーカー起動時確保・再利用。リクエスト毎 `Vec` 新規禁止
- reactor（epoll）ビルドでは **libc mmsg フォールバック**を残しコンパイル可能に

### 14.5 実装フェーズ（委譲先はこの順で進める）

#### フェーズ 0 — 調査・設計メモ（半日〜1 日、コード前に必須）

成果物: `docs/artifacts/f125_udp_iouring_design.md`（または本チケット追記）

含める内容:

1. ターゲット kernel（開発機 / 本番想定）。Multishot・buffer ring の可否
2. 受信: Multishot+pbuf vs pipelined 単発 RECVMSG×N の比較表と **採用案**
3. 送信: SENDMSG + GSO cmsg の SQE レイアウト（既存 `send_mmsg` の cmsg と等価）
4. OpTable: 現行単発 vs multishot キュー（既に残骸あり）の利用方針
5. フォールバック条件（ENOSYS / 制限 ring / reactor）
6. 測定計画: host net + tools/perf bridge、指標は req/s・p99・CPU・non-2xx

#### フェーズ 1 — 受信フル io_uring（libc recvmmsg をホットパスから除去）

**目標**: `process_datagram_segments` に渡すデータグラムを、**すべて** `IORING_OP_RECVMSG` 完了から得る。

実装案（どちらか、または段階）:

- **1a（比較的安全）**: N スロット（`mmsg_batch_size`）の msghdr/buf/cmsg を保持し、常に最大 N 個の RECVMSG を in-flight。完了したスロットを処理して再 arm（**ソフトウェアパイプライン Multishot**）。
- **1b（本命）**: `IORING_RECV_MULTISHOT` + provided buffers / buffer ring + `io_uring_recvmsg_out` で peer/GRO/payload をバッファ内に格納。

変更候補:

- `src/runtime/uring/udp_recv.rs`（全面書き換え）
- `src/http3_server.rs` メインループ（`recv_mmsg_sync` ループ削除またはフォールバック専用化）
- `executor.rs` の multishot 完了経路を **実使用** またはパイプライン用に簡略化
- 許可 opcode のコメントを実態に合わせる

受け入れ:

- HTTP/3 E2E pass
- ホットパスに `libc::recvmmsg` が残らないこと（フォールバック cfg 以外）を grep で確認
- perf が F-129 比で **非退行**（理想は向上）

#### フェーズ 2 — 送信フル io_uring（libc sendmmsg をホットパスから除去）

**目標**: `send_pending_packets` の送出を `IORING_OP_SENDMSG` に。

実装案:

- 既存 `MmsgSendScratch` の addr/iov/cmsg レイアウトを **SQE 用に固定**し、1 エントリ = 1 SENDMSG SQE
- 複数エントリを **1 回の submit（複数 SQE）** で投げ、CQE で部分失敗を処理（現行 sendmmsg の skip セマンティクスを踏襲）
- EAGAIN 相当は CQE の `-EAGAIN` / 内部 poll。`wait_writable_fd` への依存を減らす
- GSO: `UDP_SEGMENT` cmsg を従来どおり構築

変更候補:

- `src/udp/socket.rs` に `send_mmsg_uring` 系、または `runtime/uring` に UDP send ヘルパ
- `http3_server.rs` の `send_mmsg_flush`
- TCP の `SendMsgFuture`（F-59）を参考に cancel-safety / detach ガード

受け入れ:

- GSO on/off で E2E・h2load 成功
- ホットパスから `libc::sendmmsg` 除去（フォールバック除く）
- perf 非退行

#### フェーズ 3 — 真 Multishot 最適化（フェーズ 1a 採用時の昇格）

- buffer ring（`IORING_SETUP` / register pbuf ring）へ
- ENOBUFS 時の再 provide を非ブロックで
- CQE バッチ処理と quiche への供給をゼロコピーのまま

#### フェーズ 4 — ループと CPU 側の詰め

- timer/notify と UDP CQE の多重化コスト削減
- CC / window / batch の計測駆動チューニング（設定は F-129 済み）
- （任意）送信スクラッチと固定バッファの統合

### 14.6 セキュリティ・feature チェックリスト（方針 C）

- [ ] `PROXY_ALLOWED_OPCODES`: 使う opcode のみ。PROVIDE/REMOVE は **使うなら**残し、使わないなら削除検討
- [ ] docker `assets/security/seccomp.json` / epoll 用: `io_uring_*` は既存前提。UDP は既に sendmmsg/recvmmsg 許可 — **io_uring 化後は recvmmsg をデータパスから外しても、DNS 等で残るなら seccomp からは消さない**
- [ ] Landlock / 制限 ring 有効時の動作確認
- [ ] `veil_rt_reactor` では従来 libc 経路を維持（cfg 分岐）

### 14.7 測定の作法（極限を語るために必須）

1. **同一条件**で before/after（コミット直前との比較、または feature flag で新旧切替）
2. 最低 2 環境:
   - **host net + GSO on**（上限の参考）
   - **tools/perf / Docker bridge**（ドキュメント上の 835 と比較可能）
3. 記録: req/s median、p99、CPU%、non-2xx、UDP datagram sent/received（h2load）
4. 結果は `docs/perf/` または `docs/artifacts/` に残し、README 計測履歴に 1 行追記

### 14.8 委譲先への明確な指令文

> **UDP と HTTP/3 のデータプレーン I/O を、AGENTS.md のホットパス規則の範囲で極限まで io_uring 化せよ。**  
> quiche は sans-IO のまま駆動する。libc の `recvmmsg`/`sendmmsg` をホットパスの主経路から外し、`IORING_OP_RECVMSG`（可能なら Multishot + provided/buffer ring）と `IORING_OP_SENDMSG`（GSO cmsg 付き）を主経路にせよ。  
> 実装は §14.5 のフェーズ順。各フェーズで E2E と perf A/B を行い、退行させない。  
> 完了時は AGENTS.md / README / config / 本チケット / backlog.md を実装に合わせて更新せよ。

---

## 13. 変更履歴（本チケット）

| 日付 | 内容 |
|------|------|
| 2026-07-19 | F-129 実装レビューに基づき引き継ぎドキュメントとして新設。現状・ギャップ・残りタスクを固定。 |
| 2026-07-19 | §14 追加: UDP/HTTP3 io_uring 化の可否・現状層分け・極限性能フェーズ・委譲指令。方針 C をユーザー要求としてデフォルト完了条件に。 |
