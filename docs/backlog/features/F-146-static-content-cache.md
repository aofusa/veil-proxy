# F-146: 静的ファイル本体キャッシュ（HTTP/2・HTTP/3）

## 背景・実測根拠

F-145 の DTrace 実測（FreeBSD aarch64、54KB 静的ファイル）で、veil の HTTP/2 大きめ
ファイル配信のスループットが nginx の約半分にとどまることが判明していた。原因として、
HTTP/2（`src/proxy.rs` `h2_sendfile`）・HTTP/3（`src/http3_server.rs`
`handle_sendfile`）の静的配信は **リクエストごとにファイル全体を読み直していた**:

- HTTP/2: `crate::runtime::io::read(&final_path).await`
  （内部で `offload(std::fs::read)`）
- HTTP/3: `crate::runtime::offload::offload(move || std::fs::read(read_path)).await`

いずれも open/read/close のシステムコールに加え、専用オフロードスレッドプールへの
クロススレッド往復（DTrace で `_umtx_op` として観測）と新規 `Vec` 確保が固定コストと
して発生する。HTTP/1.1 は `sendfile(2)`/kTLS splice によるカーネル内ゼロコピー
（`handle_sendfile_zerocopy`）またはプールバッファのストリーミングを使っており、この
問題を持たない（本チケットの対象外）。

HTTP/2・HTTP/3 はレスポンスを DATA フレーム / QUIC ストリームへ再フレーミングする
必要があるため `sendfile(2)` は使えない。そこで代わりに **ファイル本体をユーザ空間
メモリ（`bytes::Bytes`）に保持し、参照カウントクローンで配信する**方式でこの往復を
解消する。

## 実施した改修

### 新モジュール `src/cache/content_cache.rs`

`file_cache.rs`（`OpenFileCache`）と同じ設計（`DashMap`・`cache` feature ゲート・
`cache` 無効時のスタブを `mod.rs` に用意）で、ファイル**本体**をキャッシュする層を
追加した。

- ストレージ: `DashMap<PathBuf, CachedContent>`
  （`CachedContent { data: Bytes, len: u64, mtime: Option<SystemTime>,
  mime_type: Arc<str>, cached_at: Instant }`）。
- 公開 API（モジュール内: `get_or_load`/`get_or_load_with_mime`/`invalidate`/
  `clear`/`hits`/`misses`/`len`。`cache::mod` からは
  `get_or_load_content_cache`/`get_or_load_content_cache_with_mime`/
  `invalidate_content_cache`/`clear_content_cache`/`content_cache_hits`/
  `content_cache_misses`/`content_cache_len` として再エクスポート — 汎用的な
  `get_or_load`/`invalidate` という名前がモジュール境界の外で曖昧にならないよう
  呼び出し側の名前を具体化した）。
- **キャッシュヒット時のホットパス**: `DashMap` ルックアップ + TTL 判定 +
  `Bytes::clone()`（参照カウント増加）のみ。syscall・メモリアロケーション・
  オフロードは一切発生しない。
- **キャッシュミス時**: `offload(|| std::fs::read(..))` で一度だけ読み込み、
  上限（後述）に収まる場合のみ挿入する。
- **MIME タイプの再利用**: `get_or_load_with_mime(path, cfg, mime_fallback)` は
  ヒット時に `mime_fallback` を一切呼び出さず、キャッシュ済み `Arc<str>` を返す。
  呼び出し元（HTTP/3）は `OpenFileCache` 側で既に解決済みの MIME を
  `mime_fallback` として渡すことで、ミス時にも `mime_guess::from_path` の
  再計算を避ける。

### 設定

TOML に `[static_file_cache]`（グローバル）と `[route.static_file_cache]`
（ルートごとの上書き、`OpenFileCacheConfig` と同じ Option フィールド方式）を
追加した。

```toml
[static_file_cache]
enabled = false            # 既定オフ
valid_duration_secs = 60
max_entries = 1024
max_file_size_bytes = 1048576     # 1 MiB
max_total_bytes = 67108864        # 64 MiB
revalidate_mtime = false
```

- `enabled`（既定 `false`）: 無効時は `get_or_load_content_cache` が常に
  オフロード読み込みへ内部フォールバックし、キャッシュへは一切挿入しない
  （＝挙動は F-146 適用前と同じ、追加コストもゼロ）。
- `max_file_size_bytes`（既定 1 MiB）: これを超えるファイルは常にオフロード
  読み込みにフォールバックし、キャッシュしない。
- `max_total_bytes`（既定 64 MiB）: 全エントリの `data` 合計バイト数の上限。
  超える挿入は **拒否**する（admission control のみ、LRU エビクションは実装
  していない。詳細は下記「設計判断」参照）。
- `max_entries`（既定 1024）。
- `revalidate_mtime`（既定 `false`）: 下記「設計判断」参照。

グローバル設定は `cache::configure_global_static_content_cache`（起動時・SIGHUP
リロード時の両方、`configure_global_open_file_cache` と同じ 2 箇所の呼び出し）で
ロックフリー atomic に反映する。ルートごとの上書きは `Backend::SendFile` の新規
フィールド `Option<Arc<cache::StaticContentCacheRouteConfig>>` として保持し、
リクエストごとに `cache::effective_static_content_cache_config(route_override)`
でグローバル設定とマージする（アロケーション無し）。

### 呼び出し箇所

- `src/proxy.rs` `h2_sendfile`: `crate::runtime::io::read(&final_path).await` を
  `cache::get_or_load_content_cache(&final_path, &content_cfg).await` に置き換えた。
  `build_h2_compressed_file_response` は元々 `&[u8]` を取るため、`&bytes::Bytes`
  を渡すだけでシグネチャ変更は不要（`Bytes: Deref<Target=[u8]>` の暗黙変換）。
  読み込み失敗時は既存の `cache::invalidate_file_cache` に加えて
  `cache::invalidate_content_cache` も呼ぶ。
- `src/http3_server.rs` `handle_sendfile`: 同様に置き換えた
  （`cache::get_or_load_content_cache_with_mime` で MIME 再利用込み）。

HTTP/1.1（`handle_sendfile_zerocopy`・プールバッファストリーミング）は無変更。

### HTTP/3 ホットパス違反の追加修正（同一チケットのスコープ内）

`src/http3_server.rs` `handle_sendfile` には F-146 着手前から次の問題があった:

1. **`p.is_dir()` が同期ブロッキング stat としてイベントループ上で直接呼ばれて
   いた**（ホットパス絶対規則違反）。HTTP/1.1・HTTP/2 が使っている非同期
   `cache::get_file_info_with_config(...)` による解決に置き換えた（`OpenFileCache`
   経由、無効時も内部でオフロード経由の非同期 stat にフォールバックする）。
   これにより HTTP/3 の静的配信ロジックが h1/h2 と同じ「`full_path` を組み立てて
   `get_file_info_with_config` → `!is_file` ならインデックスファイルを再解決」
   パターンに統一された。
   - **挙動差分（意図的）**: 従来はディレクトリルートでインデックスファイルが
     存在しない場合 404（`std::fs::read` 失敗）を返していたが、h1/h2 と同じ
     ロジックに統一した結果 **403 Forbidden** を返すようになった。h1/h2 は
     元々この場合 403 を返していたため、これはプロトコル間の不整合を解消する
     意図的な変更である。
2. **`mime_guess::from_path(&file_path)` が毎リクエスト実行されていた。**
   `OpenFileCache` 経由で得た MIME タイプ（`file_info.mime_type`）を
   コンテンツキャッシュのミス時フォールバックとして渡すことで、この呼び出しを
   削除した（ヒット時はコンテンツキャッシュ内の MIME をそのまま再利用、ミス時も
   `OpenFileCache` 側の解決結果を再利用するため `mime_guess` の呼び出し自体が
   消える）。
3. **`let read_path = file_path.clone();` の削減**: コンテンツキャッシュ API が
   `&Path` を借用で受け取るため、この専用クローンは不要になった（ミス時は
   `content_cache.rs` 内部で `path.to_path_buf()` を 1 回行う。これは
   `offload` クロージャに所有権を渡す必要があるための必須コストで、ミス時のみ
   発生する）。
   - **残したアロケーション**: レスポンスヘッダの
     `Vec<(Vec<u8>, Vec<u8>)>`（`header_store`）構築は、`send_response` の
     シグネチャ（`&[(&[u8], &[u8])]`）を変えずに削減する現実的な手段が無く、
     h1/h2 の既存実装も同じ構造であるため本チケットのスコープ外として残した。

### 設計判断（トレードオフの明示）

- **`revalidate_mtime` のデフォルトは `false`。** `true` にするとキャッシュ
  ヒット時に毎回 `stat`（オフロード経由）を実行して mtime を比較するため、
  ホットパスに syscall が復活する。`false` のままだとファイル更新は
  `valid_duration_secs`（TTL）が切れるまで反映されない。デフォルトは
  syscall ゼロを優先し、即時反映が必要な運用でのみ明示的に有効化する
  trade-off とした。
- **`max_total_bytes` 超過時は LRU エビクションではなく単純な admission
  control（挿入拒否）。** LRU はエントリ間の「最近使われた度合い」を継続的に
  追跡・比較するコストを伴うが、静的ファイルキャッシュは少数の高頻度アクセス
  ファイル（CSS/JS/画像等）を想定しており、単純な admission control で実運用上
  十分な効果が得られる一方、追加のロックやアトミック操作をホットパスへ持ち込ま
  ずに済む。

## 変更ファイル

- `src/cache/content_cache.rs`（新規）
- `src/cache/mod.rs`（`content_cache` モジュール宣言・再エクスポート・
  `cache` feature 無効時のスタブ）
- `src/config.rs`（`[static_file_cache]` グローバルセクション、
  `Route.static_file_cache`、`Backend::SendFile` タプルへのフィールド追加、
  `configure_global_static_content_cache` の呼び出し配線）
- `src/proxy.rs`（`h2_sendfile` の呼び出し箇所置き換え、シグネチャに
  `static_file_cache_config` 引数追加。HTTP/1.1 経路は `Backend::SendFile`
  分解のタプル arity 修正のみで無変更）
- `src/http3_server.rs`（`handle_sendfile`・`SendFileRequest` の置き換えと
  is_dir/MIME 修正）
- `examples/config.toml`（`[static_file_cache]`・`[route.static_file_cache]`
  のリファレンス追記）
- `docs/backlog/backlog.md`（本チケットの行を追加）

## 見送った項目・既知の制限

- レスポンスヘッダ `Vec<(Vec<u8>, Vec<u8>)>` の構築（`header_store`）は
  `send_response`/`h2_emit_full` のシグネチャ変更を伴わない範囲では削減できず、
  据え置いた（上記「残したアロケーション」参照）。
- 圧縮（`build_h2_compressed_file_response` の gzip/br/zstd）は圧縮後データを
  キャッシュしない（キャッシュされるのは常に非圧縮の生ファイル）。圧縮結果まで
  キャッシュする拡張は別チケット。
- HTTP/3 のディレクトリルート封じ込め検査（`cache::sendfile_base_contains`
  相当）は F-145 の時点から HTTP/3 に実装されておらず、本チケットでも追加
  していない（スコープ外、既存の挙動を変えないため）。
