# F-183: veil 専用アロケータ（veil-alloc）と、mimalloc / jemalloc / rustls / ftlog のチューニング

**状態: 完了（feat/f183-veil-allocator）**

要件: **性能を最大化しつつ、24 時間 365 日安定動作すること**（RSS が負荷の山を越えるたびに
積み上がらない・失敗要求ゼロ・スレッドの増減や fork に耐える）。

## 背景

- mimalloc / jemalloc は、アロケータが次に呼ばれたときに期限切れの空き領域を OS へ返す。負荷の後に
  アイドルになったワーカーは呼ばれないので、**ピーク時の RSS を持ち続けていた**（Linux mimalloc:
  負荷後 87.5MB、アイドル 24MB に戻らない。FreeBSD jemalloc: ソーク 40 分で底が 37 → 133MB と単調増加）。
- 参考設計書（`docs/artifacts/veil-specialized-allocator-design.ja.md`）はスレッドローカルのサイズクラス
  アロケータを提案していた。本チケットでは veil 向けに次の点を設計し直した。

## 設計（参考設計書からの変更点）

| 項目 | 参考設計書 | 本実装 |
|---|---|---|
| フリーリスト | サイズクラス単位 | **ページ（スパン）単位**（mimalloc 型）。ページが丸ごと空くのを検出でき、断片化した空きが RSS に居座らない |
| 解放時のサイズ | ポインタから引く | **`GlobalAlloc` が渡す `Layout` から計算**（ブロックヘッダ・ポインタ→サイズの表が不要） |
| 他スレッドの解放 | 遅延キュー or ロック | **所有ヒープの Treiber スタックへ CAS で投函**、所有者が遅いパス・アイドル時に一括回収（ABA 無し） |
| スレッド終了 | 言及なし | pthread キーのデストラクタで掃除し**放棄リストへ**。次のスレッドがヒープごと引き継ぐ（セグメント所有者を書き換えない）。終了処理後の確保はロック付き共有ヒープ |
| 大きな確保 | 64/128KiB 以上は OS 直接 | 256KiB までサイズクラス、**1MiB までスロット単位の large スパン**（dirty 再利用と遅延 purge に乗る）、それ以上は `mmap`/`munmap`（Linux は `mremap` で伸縮） |
| 返却 | 次の確保時のチェック | 遅延 purge（既定 1000ms）+ dirty 上限 + **ワーカーのアイドル点から `collect`**（確保が止まっても返す） |
| fork | 言及なし | `pthread_atfork` でグローバルロックを前後に獲得・解放（FreeBSD の接続ブローカー等） |
| THP | — | 試したが不採用（64KiB 単位の purge が 2MiB ページを分割し、再フォルトで 2MiB をゼロ埋めする。`live_set` で 25% 遅い） |

構造: 4MiB 境界のセグメント（64KiB スロット × 64、スロット 0 にヘッダ 3.9KB）。52 サイズクラス
（16〜128B は 16B 刻み、以降 2 倍ごとに 4 分割、内部断片化 ≤25%）。スパンはブロック 8 個以上・端数 1/8 以下
（上限 16 スロット）。満杯ページは空きが 1/8 溜まるまでキューへ戻さない（1 つ空くたびに戻すと遅いパスを
往復する。`live_set` で遅いパス 3.2M → 1.05M 回）。最初の切り出しは 4KiB だけ触る。

チューニング（環境変数、起動時に 1 回読む）: `VEIL_ALLOC_PURGE_DELAY_MS`（1000）、`VEIL_ALLOC_DIRTY_MAX_MB`
（16）、`VEIL_ALLOC_SEGMENT_CACHE`（8）。メトリクス: `veil_allocator_bytes{state}`・`veil_allocator_count{kind}`。

アイドル時回収（`src/alloc_tune.rs`）: accept の 1 秒タイムアウト・HTTP/3 のタイムアウトから `idle_tick`
（スレッドごとに 1 秒に間引く）。veil-alloc は `collect`、mimalloc は `mi_collect(false)`、FreeBSD の libc
jemalloc は `thread.tcache.flush` + `arena.4096.decay`。

## 他ライブラリのチューニング

| 対象 | 変更 | 結果 |
|---|---|---|
| mimalloc v3（Linux 既定 feature） | アイドル時回収、`purge_delay` 1000 → 250ms（`.init_array`、添字は読み戻しテストで固定） | 負荷後 RSS 87.5 → 78.8MB。スループット ±1%。`purge_delay=0` は h2c プロキシ -4.4%、`purge_decommits=0` は RSS +20%、THP・`page_full_retain`・`generic_collect`・`page_max_candidates` は差なし |
| FreeBSD libc jemalloc | `malloc_conf` シンボルで `dirty_decay_ms:1000,muzzy_decay_ms:0,retain:false,lg_tcache_max:16`、アイドル時回収 | 負荷後 RSS 102 → 52MB、ソーク 40 分の底 37→133MB（増え続ける）→ 31〜49MB（頭打ち）。要求あたり CPU 時間は同等以下。`background_thread` は libc ビルドで非対応、`narenas:4` は効果なし |
| rustls（サーバ） | `[tls] session_resumption = "tickets"`（既定）: ステートレスチケット（プロセス共有の鍵）+ TLS 1.2 session ID 用キャッシュ 4096、TLS 1.3 チケット 2 → 1 枚 | TLS 1.3 の接続チャーン 1,839 → 1,908 req/s。旧既定（256 件の共有キャッシュ）は毎秒数千ハンドシェイクで 1 秒足らずで入れ替わり、後から戻るクライアントが再開できなかった |
| ftlog | `[logging] channel_size` 100000 → 16384 | 有界チャネルは全スロットを起動時に初期化して常駐（1 件 約 88B）。アイドル RSS 24.4 → 17.3MB（-29%） |
| quiche | 変更なし | F-124/F-151 で設定可能・計測済み |

## 計測

### Linux（`tools/perf/alloc_ab.sh`、2 ワーカーを 2 コアに固定、負荷は別 2 コア、交互 A/B 3 ラウンドの中央値）

| | mimalloc（F-183 前） | mimalloc（チューニング後） | veil-alloc |
|---|---|---|---|
| h1 TLS 54KB / 3B（rps） | 10,897 / 30,574 | 10,957 / 30,314 | 10,958 / 30,699 |
| h2 TLS 3B | 31,489 | 31,889 | 32,208 |
| h1 TLS → 上流 3B / h2c → 上流 54KB | 14,760 / 13,469 | 14,636 / 13,584 | 14,700 / 13,592 |
| TLS 1.3 接続チャーン | — | 1,892 | 1,901 |
| アイドル RSS | 24.2MB | 17.4MB | 17.3MB（匿名 3.5MB） |
| 負荷の 15 秒後の RSS | 87.5MB | 78.8MB | 74.4MB |

失敗要求はすべて 0。マイクロベンチ（`tools/perf/alloc_bench`、ns/op、中央値）:

| | veil-alloc | mimalloc | system (glibc) |
|---|---|---|---|
| lifo_small | 5.6 | 6.6 | 7.1 |
| request_batch | 17.9 | 24.2 | 30.6 |
| live_set（50k 生存、混合） | 47.2 | 40.2 | 146 |
| live_small / mid / big / xl / large | 30 / 46 / 85 / 55 / 82 | 30 / 68 / 124 / 80 / 86 | 72 / 146 / 341 / 152 / 130 |
| buf64k | 13.6 | 21.8 | 35.5 |
| vec_growth | 186 | 204 | — |
| cross_thread | 12.3 | 17.2 | 55.8 |
| mt_live（4 スレッド） | 32.9 | 33.8 | 69.8 |

`live_set` だけ mimalloc が速いのは、mimalloc v3 がアリーナに `MADV_HUGEPAGE` を掛けて TLB ミスを減らすため
（`MIMALLOC_ALLOW_THP=0` で 45.2 となり veil-alloc 46.5 と同等）。veil の生存集合は数十 MB で、THP は RSS を
2MiB 単位で膨らませるため採らない（上記）。

### ソーク（`tools/perf/alloc_soak.sh`、40 分、全種の負荷をランダムに 1〜2 本同時 + 15 秒アイドルを反復）

| | サイクル | 失敗 | 最大 RSS | 底（最初 → 最後） | 後半の底の傾き |
|---|---|---|---|---|---|
| veil-alloc | 62 | 0 | 136MB | 55 → 100MB（10 サイクルで頭打ち、94〜106MB を往復） | **-29.7MB/h** |
| mimalloc（チューニング後） | 62 | 0 | 115MB | 36 → 96MB（後半も増加） | **+34.7MB/h** |

底の 100MB 前後はスレッドローカルのバッファプール（上限付き）・上流のアイドル接続・キャッシュが最大負荷の
規模まで埋まった分で、上限で止まる。veil-alloc は後半で減少、mimalloc は後半も増加が続いた。

### FreeBSD（Apple Silicon 上の QEMU/HVF の FreeBSD 14.3 aarch64、`tools/perf/bsd/alloc_ab_bsd.sh`）

VM の vCPU が効率コアに載るため rps の揺れが ±35% あり、要求あたりの veil の CPU 時間で比べた
（既定 jemalloc 比: h1 TLS 54KB -2.4%、3B -3.3%、h2 TLS 3B -1.1%、h1 プロキシ -3.4%、h2c プロキシ -4.1%、
接続チャーン -5.0%）。負荷後 RSS 102 → 52MB。ソーク 40 分（`alloc_soak_bsd.sh`）は上表のとおり。

## 結論

- **Linux は veil-alloc を使う**（unix の `full` / `full-container` の既定）。スループットはチューニング後の
  mimalloc と同等以上、負荷後 RSS は低く、ソークの定常状態で RSS が積み上がらない。マイクロベンチでは 8 種中 7 種で
  mimalloc より速い。依存は libc のみ（C のビルドが不要、メトリクスで内訳を観測できる）。
- 既定 feature（`default = ["ktls", "http2", "mimalloc"]`）は AGENTS.md の規則どおり変えない（チューニング済み
  mimalloc）。FreeBSD は libc の jemalloc をチューニングして使う。Windows は mimalloc。

## 検証

- 単体（veil-alloc 19 件、セッション再開 2 件、mimalloc 起動時オプション 1 件を含む 1025 件）・統合 54 件
  （`full` = veil-alloc がテストバイナリのグローバルアロケータ）
- E2E: io_uring 565 件、epoll 565 件、すべて成功
- `--no-default-features`・`veil-alloc` 単体・`alloc-stats` との組み合わせ・`full-container` の check、clippy
- macOS / Windows のクロスビルド、FreeBSD aarch64 の VM ビルドと計測
