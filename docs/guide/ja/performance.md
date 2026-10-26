# パフォーマンスチューニングとベンチマーク

[← ドキュメント目次](README.md) · [English](../performance.md)

## パフォーマンスチューニング

### ワーカースレッド数

ワーカースレッド数は `config.toml` の `[server]` セクションで設定できます。

```toml
[server]
listen = "0.0.0.0:443"
threads = 0  # 未指定または0の場合はCPUコア数と同じ
```

| 設定 | 動作 |
|------|------|
| 未指定 | CPUコア数と同じスレッド数 |
| `threads = 0` | CPUコア数と同じスレッド数 |
| `threads = 4` | 4スレッドで起動 |

- 各ワーカースレッドはCPUコアにピン留めされます（CPUアフィニティ）
- コア数よりスレッド数が多い場合はラウンドロビンで割り当て
- メモリ制約がある環境では少なめに設定することを推奨
- `0`／未指定は cgroup の CPU 割り当て（`cpu.max` / `cpu.cfs_quota_us`）を反映します（`--cpus 0.25` のコンテナではホストのコア数ではなく 1 ワーカー）

### メモリ使用量（B-101）

1 ワーカーの TLS コンテナ（`--cpus 0.25`）での実測です。

| | v0.7.0 | v0.8.0 |
|---|---|---|
| アイドル時のプロセス（anon） | 約 25MB | 約 10MB |
| キープアライブ待ちの TLS 接続 1 本 | 約 100KB | 約 33KB |
| `wrk -c400`（静的配信・TLS）中のピーク | 約 100MB | 約 58MB |

- 次の要求を待つキープアライブ接続は読み取りバッファを持ちません。readiness を待ってから、データが届いた時点で 64KB のバッファを借ります。要求の蓄積バッファは初期 8KB で、待機中はスレッドローカルのプールへ返します。
- io_uring では、平文の HTTP/1.1 の接続で応答を返した直後に、次の要求を 8KB の蓄積バッファへ `[performance] keepalive_direct_read_ms`（既定 20ms、F-205）の間だけ直接読みにいきます。その間に次の要求を送るクライアントは、`POLL_ADD` + `RECV` の代わりに `RECV` 1 回で、読み取りバッファからの写しも無しに処理されます（小さな API の応答で req/s +7〜8%）。期限を過ぎると読みを取り消し、バッファをプールへ返して上のとおりバッファを持たずに待ちます。`0` で無効。TLS の接続と reactor（epoll・kqueue・WSAPoll）は常に先に待ちます。
- スレッドローカルのバッファプールは遅延確保で、保持数に上限（各 64）があります。
- mimalloc のアリーナを一括コミットしません（`arena_eager_commit = 0` を `main` より前に設定）。スループット差なしで常駐メモリが約 15MB 減ります。環境変数 `MIMALLOC_ARENA_EAGER_COMMIT` を指定するとそちらが優先されます。
- 目安: 64MB のコンテナで TLS のアイドル接続 600 本と処理中の 400 接続を維持できます。さらに小さなコンテナでは `[security] max_concurrent_connections` で上限を設けてください。

### メモリアロケータ（F-183）

| プラットフォーム / ビルド | グローバルアロケータ | チューニング |
|---|---|---|
| Linux、`--features full`（と `full-container`） | **veil-alloc**（`src/veil_alloc/`、推奨） | veil の thread-per-core ワーカー向けの専用設計（下記） |
| Linux、既定 feature | mimalloc v3 | `arena_eager_commit = 0`・`purge_delay = 250` ms（`main` 前に設定。`MIMALLOC_*` 環境変数が優先） |
| FreeBSD（`full-freebsd`） | libc の `malloc(3)`（jemalloc） | `malloc_conf` シンボルで `dirty_decay_ms:1000,muzzy_decay_ms:0,retain:false,lg_tcache_max:16`（`MALLOC_CONF`・`/etc/malloc.conf` が優先） |
| OpenBSD / NetBSD | libc の `malloc(3)` | — |
| Windows（`full-windows`。F-184） | mimalloc v3 | アイドル時回収（`mi_collect`） |
| macOS の配布ビルド（`full-macos`。F-184） | システムアロケータ（libmalloc） | — |

どのアロケータにも **アイドル時回収** をかけます。ワーカーの accept ループは接続が来ないと 1 秒で
タイムアウトし（HTTP/3 はアイドル時のタイムアウト）、そこで使用中のアロケータに空き領域を OS へ
返させます（`veil_alloc::collect`、`mi_collect`、jemalloc の `thread.tcache.flush` +
`arena.<all>.decay`）。これが無いと mimalloc・jemalloc は次に呼ばれるまで返却しないため、負荷の
後にアイドルになったワーカーはピーク時の RSS を持ち続けていました。

**veil-alloc** はスレッドローカル・サイズクラス・ページ単位フリーリストのアロケータです。

- 高速パスは現在のページのフリーリストへの pop / push だけで、ロックもアトミックもありません。
  `GlobalAlloc` は解放時にもサイズを渡すので、ブロックごとのヘッダやポインタ→サイズの表を持ちません。
- 64KiB スロットからなる 4MiB セグメント。256KiB までは 52 のサイズクラス、1MiB まではスロット単位の
  スパン、それより大きい要求は `mmap` し、解放で `munmap` します。
- 他スレッド（ロガー・オフロード・チャネル越し）の解放は所有ヒープへロックフリーに投函し、所有者が
  回収します。
- 空いたスパンは `VEIL_ALLOC_PURGE_DELAY_MS`（既定 1000）だけ常駐させてから `madvise` で返します。
  ヒープあたりの常駐した空き領域に上限（`VEIL_ALLOC_DIRTY_MAX_MB`、既定 16）、空セグメントの
  プロセス全体キャッシュは `VEIL_ALLOC_SEGMENT_CACHE`（既定 8）個までです。
- 終了したスレッドのヒープは次のスレッドが引き継ぐので、スレッドの入れ替わりでメモリは増えません。
  `fork(2)` の前後でグローバルロックを取ります。
- Prometheus（`metrics`）: `veil_allocator_bytes{state="active|dirty|huge|purged_total"}`、
  `veil_allocator_count{kind="segments|cached_segments|heaps|abandoned_heaps|remote_frees_total"}`。

`tools/perf/alloc_ab.sh` での計測（Linux、2 ワーカーを 2 コアに固定、負荷は別の 2 コアから、交互 A/B
5 ラウンドの中央値。TLS 上の HTTP/1.1・HTTP/2 の静的配信とプロキシ、接続チャーン。失敗要求はすべて 0）:

| | mimalloc（F-183 前） | mimalloc（チューニング後） | veil-alloc |
|---|---|---|---|
| スループット | 基準 | -0.1〜+1.1% | -0.3〜**+4.2%**（HTTP/2 TLS 3B） |
| 要求あたりの veil の CPU 時間 | 基準 | -1.0〜+1.1% | **-1.6〜-4.1%** |
| アイドル RSS | 24.0 MB | 17.1 MB | 16.8 MB（匿名メモリ 3.5 MB） |
| 負荷中のピーク RSS | 基準 | -12〜-17% | -13〜-21% |
| 負荷の 30 秒後の RSS | 87.4 MB | 75.0 MB | 73.8 MB |

両者に同じ乱数の種の負荷列を与えた 40 分のソーク（`tools/perf/alloc_soak.sh`: 全種の負荷をランダムに
最大 2 本同時 + 15 秒アイドルの反復）は、どちらも失敗要求 0 でした。アイドル後の RSS の底は、上限付きの
プール・キャッシュが最大負荷の規模まで埋まる間だけ上がり、その後は頭打ちです（最後の底は veil-alloc
85 MB、mimalloc 87 MB。定常状態の傾きはどちらも下向き）。FreeBSD の既定の jemalloc 設定では同種の
ソークで底が上がり続け（+27 MB/h）、チューニングした `malloc_conf` では横ばいでした。

その後、io_uring のタイマーヒープがキャンセル済みのタイムアウトを期限まで抱え、「要求レート × タイムアウト秒数」に
比例して膨らんでいたのを直しました（B-117）。上の負荷列で負荷中のピーク RSS が 19〜40% 下がり、負荷後の RSS は
75 MB から 45 MB になりました（スループット・要求あたり CPU は同等）。接続チャーン・異常切断・設定リロードの反復で
メモリや fd が増え続けないことは `tools/perf/leak_check.sh` で確かめています（F-185）。

アイドル時の減少の大半は `[logging] channel_size` によるものです（ftlog は全スロットを事前確保する。
既定を 100000 から 16384 に変更、-7 MB）。`tools/perf/alloc_bench` でアロケータを直接比べると、
veil-alloc は小物・要求単位のバッチ・64KiB バッファ・スレッド間解放・`realloc` で mimalloc より
18〜60% 速く、4 スレッドの混合した生存集合では同等です。FreeBSD ではチューニングした `malloc_conf`
で負荷後の RSS が 102 MB から 52 MB になり、要求あたりの CPU 時間は同等以下でした。

### SO_REUSEPORT CBPFロードバランシング

#### 概要

SO_REUSEPORTを使用して複数のワーカースレッドが同一ポートをリッスンする際、デフォルトではLinuxカーネルが3元タプルハッシュ（protocol + source IP + source port）で接続を振り分けます。CBPFモードでは、フローハッシュ（`skb->hash`。送信元/宛先IP・ポートの4タプルから計算）に基づいてワーカーを選択するカスタムBPFプログラムをカーネルにアタッチします（ハッシュが未計算の場合は受信CPU番号にフォールバック）。多数のクライアントからの接続を固定ワーカーへ振り分けたい場合の選択肢であり、既定は `kernel` のままです。

#### 効果

| 項目 | Kernel（デフォルト） | CBPF |
|------|---------------------|------|
| 振り分けキー | protocol + src IP + src port | フローハッシュ（4タプル）。未計算時は受信CPUへフォールバック |
| 同一接続 | 接続中は固定 | 常に同じワーカー |
| CPUキャッシュ効率 | 中 | 同等（どちらも接続を 1 ワーカーへ固定する） |
| TLSセッション再開 | 中 | 同等 |

> **注記（B-76）**: カーネル既定も 4 タプルをハッシュするため、実際には `"cbpf"` は
> `"kernel"` とほぼ同じ挙動になる。本モードの意義は「振り分けポリシーを自前の
> プログラムとして持てること」と「`skb->hash` が使えない場合に受信 CPU へ
> フォールバックできること」にある。**B-76 以前の本モードは壊れており、
> プログラムが常に 0 を返していたため全接続がワーカー 0 に固定され、
> マルチワーカーの並列性が完全に失われていた。**

#### 設定

```toml
[performance]
# "kernel" = カーネルデフォルト【既定】
# "cbpf"   = フローハッシュ（4タプル）ベースのCBPF（同一接続を固定ワーカーへ振り分け）
reuseport_balancing = "cbpf"
```

#### 要件

- **Linux 4.6以上**（SO_ATTACH_REUSEPORT_CBPFサポート）
- CBPFアタッチ失敗時は自動的にカーネルデフォルトにフォールバック

### Huge Pages（Large OS Pages）

#### 概要

mimallocアロケータでHuge Pages（2MB）を使用することで、TLB（Translation Lookaside Buffer）ミスを削減し、パフォーマンスを向上させます。

#### 効果

| 項目 | 効果 |
|------|------|
| TLBミス | 大幅削減（ページテーブル参照の減少） |
| ページフォルト | 大容量メモリ使用時に減少 |
| パフォーマンス | 5-10%向上（ワークロード依存） |
| kTLS/splice | カーネル連携時に特に効果的 |

#### 設定

```toml
[performance]
huge_pages_enabled = true
```

#### OSレベルの設定（Linux）

```bash
# 一時的にHuge Pagesを有効化（128ページ = 256MB）
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# 永続化（/etc/sysctl.conf）
echo "vm.nr_hugepages=128" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p

# 現在のHuge Pages状態を確認
grep -i huge /proc/meminfo
```

#### コンテナ環境での注意

Docker/Kubernetes環境では、ホスト側でHuge Pagesを事前に確保する必要があります：

```bash
# ホスト側でHuge Pagesを確保
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# Docker起動時（オプション）
docker run --shm-size=256m ...

# Kubernetes（Pod仕様に追加）
# resources.limits.hugepages-2Mi: "256Mi"
```

Huge Pagesが利用できない場合は、自動的に通常の4KBページにフォールバックします。

### システム設定

veil は起動時に `RLIMIT_NOFILE` の soft limit を hard limit まで自動で引き上げます（nginx の `worker_rlimit_nofile` 相当）。実効的なファイルディスクリプタ上限は、systemd の `LimitNOFILE` や docker の `--ulimit nofile` で hard limit を設定して制御してください。

```bash
# ファイルディスクリプタ上限
ulimit -n 65535

# カーネルパラメータ
sysctl -w net.core.somaxconn=65535
sysctl -w net.ipv4.tcp_max_syn_backlog=65535
sysctl -w net.core.netdev_max_backlog=65535

# io_uringの設定（必要に応じて）
sysctl -w kernel.io_uring_setup_flags=0
```

veil は listen の待ち行列を 1024 で作りますが、カーネルが上限で切ります。BSD の既定は 128 で、NetBSD は溢れた SYN に
リセットを返すので、新しい接続が一度に来ると失敗します。上限を上げてください:

```bash
sysctl kern.ipc.soacceptqueue=1024   # FreeBSD
sysctl kern.somaxconn=1024           # OpenBSD
sysctl -w kern.somaxconn=1024        # NetBSD
```

OpenBSD では既定のログインクラスが root でもファイル数を 1024 に制限します。`/etc/login.conf` の `openfiles-max` を上げてください。

### FreeBSD: h2c の静的配信のゼロコピー（F-240）

FreeBSD では、静的コンテンツキャッシュ（`[static_file_cache]` + `[route.open_file_cache]`）は 16KB 以上のファイルの本体を
無名の共有メモリに置きます。平文の h2c の応答は、DATA フレームの本体を書き込みバッファへコピーせず、`sendfile(2)`（9 バイトの
フレームのヘッダは `sf_hdtr`）で `TCP_NOPUSH` を立てて送ります。TLS の接続は従来どおりコピーします。FreeBSD 14.3 aarch64 の
実測（54KB、計測対象 1 コア）で nginx の 1.33 倍、サーバの CPU は 1 要求あたり 6.2µs（nginx 8.6µs）。

### バッファサイズとタイムアウト

コード内の定数（コンパイル時に設定、再ビルドが必要）：

```rust
// バッファサイズ
const BUF_SIZE: usize = 65536;           // 64KB - io_uring最適サイズ
const HEADER_BUF_CAPACITY: usize = 512;  // HTTPヘッダー用
const MAX_HEADER_SIZE: usize = 8192;     // 8KB - ヘッダーサイズ上限
const MAX_BODY_SIZE: usize = 10485760;   // 10MB - ボディサイズ上限

// タイムアウト
const READ_TIMEOUT: Duration = Duration::from_secs(30);   // 読み込みタイムアウト
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);  // 書き込みタイムアウト
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10); // バックエンド接続タイムアウト
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);   // Keep-Aliveアイドルタイムアウト
```

> **注意**: ルートごとのセキュリティ設定で `client_header_timeout_secs` や `backend_connect_timeout_secs` を設定することで、一部のタイムアウトはconfig.tomlから個別に調整可能です。

### バッファプール設定

バッファプールは起動時にバッファを事前確保することで、メモリアロケーションのオーバーヘッドを削減します。`[buffer_pool]`セクションで設定できます：

```toml
[buffer_pool]
# 読み込みバッファサイズ（バイト）
# デフォルト: 65536 (64KB)
read_buffer_size = 65536

# 読み込みバッファ初期プール数
# デフォルト: 32
initial_read_buffers = 32

# 読み込みバッファ最大プール数
# デフォルト: 128
max_read_buffers = 128

# リクエスト構築バッファサイズ（バイト）
# デフォルト: 1024 (1KB)
request_buffer_size = 1024

# リクエスト構築バッファ初期プール数
# デフォルト: 16
initial_request_buffers = 16

# 大容量リクエストバッファサイズ（バイト）
# デフォルト: 4096 (4KB)
large_request_buffer_size = 4096

# パス文字列バッファサイズ（バイト）
# デフォルト: 256

# レスポンスヘッダーバッファサイズ（バイト）
# デフォルト: 512
```

**注意**: バッファプール設定はオプションです。デフォルト値はほとんどの用途に最適化されています。特定のメモリ制約やパフォーマンス要件がある場合のみ調整してください。

## ベンチマーク

```bash
# wrk を使用したベンチマーク
wrk -t4 -c100 -d30s https://localhost/

# kTLS有効/無効での比較

# 1. kTLS無効（rustls使用）
cargo build --release
./veil -c ./examples/config.toml &
wrk -t4 -c100 -d30s https://localhost/

# 2. kTLS有効（rustls + 独自kTLSモジュール使用）
cargo build --release --features ktls
# config.tomlでktls_enabled = true
./veil -c ./examples/config.toml &
wrk -t4 -c100 -d30s https://localhost/
```
