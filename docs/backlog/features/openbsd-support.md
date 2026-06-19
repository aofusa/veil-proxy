# フェーズ 2: OpenBSD 対応

## 目的

Veil の高セキュリティな設計思想に極めて親和性が高い OpenBSD への対応を行う。OpenBSD はセキュリティに特化した OS であり、Linux の seccomp や Landlock の代わりとして、強力かつシンプルな `pledge` および `unveil` を備えている。これらを活用し、最も堅牢なリバースプロキシの一つとして動作させる。

## 現状と課題

現状のアーキテクチャは Linux カーネルの機能に依存しているため、OpenBSD に移植するにあたって以下の機能の代替またはフォールバックが必要となる。
- **非同期 I/O**: `io_uring` (monoio)
- **TLS オフロード**: OpenBSD には kTLS 機能がない
- **セキュリティ**: `seccomp` (システムコール制限)、`Landlock` (ファイルシステム制限)
- **ソケット負荷分散**: `SO_REUSEPORT` + CBPF
- **ゼロコピー転送**: OpenBSD には `sendfile` がない

## 改修内容・設計（マッピング）

Linux の各機能を以下のように OpenBSD の機能・制約に合わせて調整する。

1. **非同期 I/O (kqueue)**
   - OpenBSD にネイティブな `kqueue` を利用する。
   - `monoio` のレガシーバインディングを使用するか、イベントループを抽象化し kqueue で処理する。
   - tokio を使わないデータプレーンの制約は維持する。

2. **TLS オフロード (rustls へのフォールバック)**
   - OpenBSD では kTLS がサポートされていないため、常に純粋なユーザー空間の `rustls` にフォールバックして TLS 処理を行う。
   - kTLS を要求する設定であっても、OpenBSD では無視（または警告を出力して fallback）する。

3. **セキュリティ・サンドボックス (pledge & unveil)**
   - **`pledge(2)`**: システムコール制限。起動後、ネットワーク通信とログ出力程度に権限を絞り込む。例: `pledge("stdio rpath inet", "")`。
   - **`unveil(2)`**: ファイルシステム制限。設定ファイルや静的コンテンツのディレクトリのみを Read-only で unveil し、その後 unveil をロックする。
   - Linux の seccomp / Landlock よりも簡潔なコードで、同等以上の強固なサンドボックスが実現できる。

4. **ソケット負荷分散 (SO_REUSEPORT)**
   - CBPF は使用できないため、OpenBSD 標準の `SO_REUSEPORT` を用いてカーネルによる簡易なコネクションロードバランシングを行う。

5. **ファイル転送 (sendfile の代替)**
   - OpenBSD は `sendfile(2)` を持たないため、`open_file_cache` を活用しつつ、標準的な `read(2)` / `write(2)` （またはメモリマッピング）によるチャンクベースの配信にフォールバックする実装とする。

## 改修案（ステップ）

1. `cfg(target_os = "openbsd")` の導入と OS 依存コードの整理。
2. OpenBSD 上で kqueue を用いて非同期データプレーンが動作することを確認（`io_uring` 呼び出しの除外）。
3. `pledge` および `unveil` を用いたセキュリティ初期化ロジック（`openbsd_sandbox.rs` 等）の実装。
4. kTLS と sendfile が利用できない前提での、静的ファイル配信パフォーマンスの最適化（バッファプールの調整など）。
5. 必要なカーネルパニックやメモリリークに対する OpenBSD 環境での安定性テスト。

## 受け入れ条件

- OpenBSD 上で `cargo build` が成功する。
- 実行時に `pledge` と `unveil` が正しく適用され、権限外のファイルへのアクセスが OS によって `SIGABRT` などで強制終了されることが確認できる。
- kTLS や sendfile に依存せず、rustls とユーザー空間のファイルコピーで HTTP/1.1・HTTP/2 の通信が正常に行える。

## 依存・リスク

- 非同期 I/O におけるパフォーマンスが io_uring に比べて低下する可能性が高い。OpenBSD での用途はスループットよりもセキュリティが主目的となることを README 等で明記する。
- 既存の Linux 依存のテストコードが OpenBSD でパスしない場合、テストの条件付きコンパイル（`#[cfg(not(target_os = "openbsd"))]` 等）を適切に行う必要がある。
