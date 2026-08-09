# F-149 起動時ログの重複排除と誤記修正

- ステータス: **完了**
- 優先度: 低
- 関連: F-130（HTTP/3 io_uring パイプライン）、F-136（非 Linux の in-memory `SSL_CTX`）、F-28（monoio 削除）

## 背景

起動時ログを実測（`full` features・HTTP/1.1 + H2 + H2C + HTTP/3 + L4 + WASM + Prometheus + admin 構成）で
採取したところ、**同じ事実を 2 箇所以上で出力している行**と、
**現在のコードと矛盾する内容の行**が混在していた。
メッセージ本文は変えず、重複行の出力処理そのものを削る（意味は減らさない）。

## 実測ログ（改修前・抜粋、`docs/artifacts/f149_startup_log_before.log` に全文）

```
[src/config.rs:5296] HTTP/2 enabled via ALPN negotiation (h2, http/1.1)
...
[src/entry.rs:423]   HTTP/2 enabled via ALPN negotiation                 ← 重複
[src/entry.rs:431]   H2C (HTTP/2 Cleartext) enabled (listener: 127.0.0.1:8081)
[src/entry.rs:435]   HTTP/3 enabled (UDP listener: 127.0.0.1:8443)
[src/entry.rs:454]   Listen Address: 127.0.0.1:8443
[src/entry.rs:456]   CPU Affinity: Enabled (pinning workers to cores)
[src/entry.rs:518]   CPU Affinity: 4 cores available, pinning 1 worker threads
[src/entry.rs:838]   Listen Address: 127.0.0.1:8443                      ← 454 と重複
[src/entry.rs:1260]  HTTP/3 Listen Address: 127.0.0.1:8443 (UDP)         ← 435 と重複
[src/entry.rs:1272]  TLS loading method: memfd (Landlock compatible)
[src/entry.rs:1363]  H2C Listen Address: 127.0.0.1:8081                  ← 431 と重複
[src/http3_server.rs:3802] [HTTP/3] Loading certificates (via memfd/temp file (path-based quiche API))
[src/http3_server.rs:3892] [HTTP/3] Server listening on 127.0.0.1:8443 (QUIC/UDP, monoio io_uring)
[src/system.rs:544]  [Security] TLS certificate (LoadedConfig) securely zeroed (546 bytes)
[src/system.rs:544]  [Security] TLS private key (LoadedConfig) securely zeroed (241 bytes)
[src/entry.rs:1529]  [Security] Pre-loaded TLS credentials have been securely cleared from memory
```

## 改修内容

### A. 重複行の削除

| 削除する行 | 同じ意味を残す行 |
|---|---|
| `entry.rs` `HTTP/2 enabled via ALPN negotiation` | `config.rs` の `... (h2, http/1.1)`（ALPN リスト付きで上位互換） |
| `entry.rs` `H2C (HTTP/2 Cleartext) enabled (listener: X)` | H2C サーババナーの `H2C Listen Address: X` |
| `entry.rs` `HTTP/3 enabled (UDP listener: X)` | HTTP/3 サーババナーの `HTTP/3 Listen Address: X (UDP)` |
| メインバナーの `Listen Address: X` | `HTTPS Server` バナーの `Listen Address: X`（TLS リスナーを実際に起動した箇所でのみ出るため、H2C 専用構成で誤った表示にならない） |
| `entry.rs` `[Security] Pre-loaded TLS credentials have been securely cleared from memory` | `system.rs` の cert / key それぞれの `securely zeroed (N bytes)` 2 行（バイト数付きで上位互換） |

### B. 誤った内容の修正

1. **`CPU Affinity: Enabled (pinning workers to cores)`（`entry.rs`）を削除**。
   これは設定値ではなく無条件の固定文字列で、**コア ID を取得できない環境でも
   「Enabled」と出てしまう**（実際にはピン留めされない）。
   直後の `CPU Affinity: N cores available, pinning M worker threads` /
   `CPU Affinity: Could not detect core IDs, workers will not be pinned`
   が実態を正しく報告しているため、こちらだけを残す。

2. **`TLS loading method: memfd (Landlock compatible)`（`entry.rs`）を削除**。
   memfd 経由のパス指定ロードは **Linux 限定**の経路で、
   FreeBSD/macOS/Windows/OpenBSD/NetBSD は F-136 以降 in-memory `SSL_CTX`
   （`with_boring_ssl_ctx_builder`）を使うため **非 Linux では虚偽**だった。
   実際にロードした方式は `http3_server.rs` の
   `[HTTP/3] Loading certificates (via ...)` が実態に即して出力している。

3. **`[HTTP/3] Server listening on X (QUIC/UDP, monoio io_uring)` から `monoio` を削除**。
   monoio は F-28 で除去済みで、現在の UDP データプレーンは
   `src/runtime/` の独自 io_uring 実装（F-130 でパイプライン化）である。
   `(QUIC/UDP, io_uring)` へ修正する。

### C. HTTP/3 ワーカーごとの同一行の重複出力を 1 回に集約

`http3_server.rs` の以下の行は **HTTP/3 ワーカーごとに実行**されるため、
`num_threads` が大きい構成では同一内容が N 回並ぶ（例: 8 ワーカーで 56 行）。
`std::sync::Once` で最初の 1 回だけ出力する（内容はワーカー間で同一のため意味は減らない）。

- `[HTTP/3] Loading certificates (via ...)`
- `[HTTP/3] Certificates loaded, sensitive data zeroed`
- `[HTTP/3] quiche transport: cc=... pacing=... hystart=... mmsg_batch=...`
- `[HTTP/3] GSO enabled: ..., GRO enabled: ... (config gso_gro_enabled: ...)`
- `[HTTP/3] Server listening on ... (QUIC/UDP, io_uring)`
- `[HTTP/3] pipelined io_uring RECVMSG enabled (...)`
- `[HTTP/3] pipelined io_uring SENDMSG enabled (...)`

ワーカー個別の事象を示す行（`[HTTP/3 Worker N] Pinned to CPU core ...` /
`Starting...` / `Stopped` / エラー）は従来どおり全ワーカーで出力する。

## 残置（重複に見えるが意味が異なるため残すもの）

- `... thread started` 系（設定リロード / TLS 証明書リロード / ヘルスチェック /
  キャッシュクリーンアップ / WASM tick）は、それぞれ**別のスレッドが実際に起動したこと**を
  示す 1 行で、内容は重複していない。
- `SIGHUP handler registered ...`（シグナルハンドラ登録）と
  `Runtime configuration initialized (hot reload enabled via SIGHUP)`（設定の初期化完了）は別事象。
- L4 の `starting <proto> listener on ADDR (...)` と `listening on ADDR` は
  「開始要求」と「bind 成功」で別事象。
- `Threads: N (CPU cores: M)`（プロセス全体のワーカー数）と各サーババナーの
  `Workers: N (SO_REUSEPORT enabled)`（そのリスナーの多重化方式）は、
  値が一致するだけで示す事実が異なるため残す。

## 対象外

`src/main.rs` / `src/http3_server.rs` などの **doc コメント**に残る `monoio` の記述は
起動時ログではないため本チケットの対象外（依頼範囲外のドライブバイ修正を避ける）。
