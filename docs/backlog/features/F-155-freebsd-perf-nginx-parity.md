# F-155: FreeBSD の対 nginx パフォーマンスパリティ（ハーネス適正化 + reactor/静的配信の固定費削減）

- 優先度: P1
- 状態: 対応中
- 関連: F-141（FreeBSD kqueue/sendfile）、F-145（reactor E2E の空白地帯）、F-153/F-154/B-65（静的配信の固定費）、F-123（capsicum capability mode の dirfd 相対化）、B-63（aio 除外）

## 背景

FreeBSD 14.3 aarch64（QEMU/HVF）の実測（`docs/perf/README.md` の 2026-08-16 節）で、
veil は 54KB 応答で対 nginx **0.51〜0.58**、小応答（3B）で **0.28〜0.45** に留まっていた。
`docs/artifacts/freebsd_perf_bottleneck_analysis.md` で 10 の仮説を立て、実コードと
nginx 実装を突き合わせて仕分けした結果を、本チケットで実装する。

## 実施内容

| Phase | 内容 | 対象 |
|---|---|---|
| 0 | 計測ハーネスの veil 既定を `ktls_enabled = false` にする | `tools/perf/freebsd/run_perf_freebsd.sh` |
| 1 | `ProxyTarget` に事前解決済み `socket_addr` を持たせ、接続ホットパスから `to_socket_addrs()` を排除 | `src/config.rs`, `src/proxy.rs` |
| 2 | capsicum 静的パス解決の `CString::new`（1 リクエスト 1 malloc）をスタックバッファ化 | `src/security.rs` |
| 3 | kqueue `EVFILT_WRITE` の `write_hint` を導入し `Writable`/`WritableFd` の確認 `poll(2)` を省略 | `src/runtime/reactor/{poller,executor}.rs`, `tcp/unix.rs` |
| 4 | nginx の `multi_accept` 相当の協調的バッチ accept（1 周回最大 32 件） | `src/runtime/reactor/tcp/unix.rs`, `src/entry.rs` |
| 5 | FreeBSD `sendfile(2)` の `sf_hdtr` でヘッダ + 本体を 1 syscall 化 | `src/runtime/reactor/sendfile.rs`, `src/proxy.rs` |
| 6 | `TCP_NOPUSH` の RAII ガード（`NoPushGuard`） | `src/runtime/reactor/tcp/unix.rs`, `src/proxy.rs` |

## 実測で分かったこと（重要）

### Phase 0 だけで 54KB の劣後がほぼ解消した

FreeBSD の software kTLS は TLS レコード（16KB）ごとにカーネルワーカースレッドへ
暗号処理をディスパッチするため、コンテキストスイッチが秒間 40 万回に達して帯域が
直列化する（この事実自体は 2026-08-08 に `docs/perf/README.md` へ記録済みだった）。
にもかかわらず **計測ハーネスは `ktls_enabled = true` をハードコードしたままで、
nginx 側は kTLS を使っていなかった**（`ssl_conf_command` 未指定）。
つまり「54KB で nginx の半分」は **ハーネスの不公平な設定**であって、コードの欠陥ではなかった。

同一 VM・同一セッションでの実測（3 反復の中央値、54,576B）:

| シナリオ | nginx | veil | veil/nginx | 従来記録（kTLS 有効） |
|---|---|---|---|---|
| HTTP/1.1 TLS | 23,890 | 26,024 | **1.09** | 0.51 |
| HTTP/2 TLS | 21,566 | 24,922 | **1.16** | 0.56 |
| h2c 平文 | 66,724 | 40,381 | 0.61 | 0.58 |
| HTTP/3 | 3,426 | 3,428 | 1.00 | 0.98 |
| HTTP/1.1 proxy | 16,233 | 16,681 | **1.03** | 0.78 |
| HTTP/2 proxy | 12,702 | 16,191 | **1.27** | 0.98 |
| L4 TCP | 36,209 | 22,770 | 0.63 | 0.58 |

### 分析ドキュメントの H7（プロキシ p99 スパイクの原因は同期 DNS）は誤診だった

分析は `h1_proxy_tls` の p99 = 3810ms を「`TcpStream::connect_str` の
`to_socket_addrs()` が同期 `getaddrinfo` を呼んでイベントループを止めるため」と結論していたが、

1. Rust 標準の `impl ToSocketAddrs for str` は **まず `SocketAddr` としてのパースを試し、
   成功したら resolver を一切呼ばない**。上流は `127.0.0.1:18080` の IP リテラルなので
   `getaddrinfo` は元から呼ばれていない。
2. 同一 VM で測り直すと p99 は 54KB で 5.66〜5.82ms（nginx 5.87〜6.05ms）、
   小応答で 1.24〜1.30ms であり、**3810ms のスパイクは再現しない**。

Phase 1 の変更自体は「接続ごとの `Vec` 確保（`to_socket_addrs` の戻り値）を消す」という
ホットパス絶対規則上の正しい改善なので入れるが、**p99 の改善を期待してはならない**。
3810ms は別要因（計測時のホスト側の同居負荷が疑わしい）と考えられ、原因は未特定。

### Phase 5（`sf_hdtr`）が効くのは平文 HTTP/1.1 だけで、既存ハーネスでは測れなかった

FreeBSD の `sendfile(2)` ゼロコピー経路は `handle_sendfile_userspace` の
`is_plain()` 分岐だけであり、

- rustls ユーザー空間 TLS: 平文漏洩になるので通さない（意図的）
- HTTP/2・h2c: HEADERS/DATA フレーミングが要るので `sendfile` に載らない

したがって `sf_hdtr` の効果は **平文 HTTP/1.1 の静的配信でしか観測できない**。
既存の `ALL_SCENARIOS` にその構成が無かったため、**検証用に `h1_file_plain`
シナリオを追加した**（veil の平文リスナー / nginx の平文 server へ wrk で
HTTP/1.1 を投げる。nginx 側も `sendfile on; tcp_nopush on;` なので
「1 リクエストあたりの syscall 数」を正面から比較できる）。

## 設計上の判断（分析ドキュメント・実装指示書から意図的に外したもの）

1. **`Writable::poll` の `poll(2)` フォールバックは残す。** 実装指示書は
   `take_write_hint` が 0 のとき即 `register_write` + `Pending` を返す形にしていたが、
   これは `Readable::poll`（F-141）の設計と非対称で、ヒントが無いとき必ず kqueue
   往復 1 回分のレイテンシが乗る。`Readable` と同じ「ヒント → `poll(2)` → 登録」の
   3 段にした。
2. **capsicum のパス長超過は `CString` へフォールバックしない。** 指示書は
   1024B 超で `CString::new` へ落とす案だったが、`openat`/`fstatat` はどのみち
   `ENAMETOOLONG` になるため、ホットパスにアロケーション経路を残す意味がない。
   `None` を返して通常経路へ委ねる。相対パス中の NUL バイト検査は
   （`CString::new` が担っていた安全性なので）明示的に残す。
3. **`sf_hdtr` ループに `nbytes == 0` ガードを置く。** FreeBSD の `sendfile(2)` は
   `nbytes == 0` を「ファイル終端まで送る」と解釈する。`sf_hdtr` はヘッダを本体より
   先に送るので「本体を送り切ったのにヘッダが残る」状態は原理上ありえないが、
   万一その不変条件が破れると `Content-Length` を超えるバイト列を送って
   レスポンスを破壊するため、検出して `Err` で打ち切る。
4. **`NoPushGuard` は `TcpStream` の借用に縛る。** `Drop` で fd へ `setsockopt`
   するため、`TcpStream` が先に close されると再利用された fd を触りうる。
   `PhantomData<&'a TcpStream>` でコンパイル時に防ぐ。

## 未解決（本チケットの範囲外・要フォローアップ）

- **h2c 平文 54KB が 0.61、L4 TCP 54KB が 0.63。** どの Phase も直接は狙っていない。
  h2c は B-65 の続きであり、L4 は splice/中継経路の帯域。
- 小応答（3B）のリクエスト単価。Phase 2/3/4 が効く想定だが、効果の確定は A/B 実測による。
