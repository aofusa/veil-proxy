# フェーズ 2: FreeBSD 対応

## 目的

Veil の高パフォーマンス・高セキュリティなアーキテクチャを維持しつつ、FreeBSD ネイティブな機能を活用して FreeBSD プラットフォームへの対応を行う。Linux 固有の機能（io_uring、seccomp、Landlock 等）を FreeBSD の同等機能（kqueue、Capsicum 等）にマッピングし、OS の持ち味を最大限に引き出す。

## 現状と課題

現状のアーキテクチャは Linux カーネルの機能に強く依存している。
- **非同期 I/O**: `io_uring` (monoio)
- **TLS オフロード**: Linux kTLS (`ktls2`)
- **セキュリティ・サンドボックス**: `seccomp` (システムコール制限)、`Landlock` (ファイルシステム制限)
- **ソケット負荷分散**: `SO_REUSEPORT` + CBPF
- **ゼロコピー転送**: Linux `sendfile(2)` または `splice(2)`

## 改修内容・設計（マッピング）

Linux の各機能を以下のように FreeBSD のネイティブ機能に置き換える設計とする。

1. **非同期 I/O (kqueue)**
   - `monoio` のレガシーバインディング（`mio`/`kqueue` ベース）を活用するか、FreeBSD 用の `kqueue` 抽象化層を導入する。
   - スレッドごとのスループットを維持するため、`tokio` への移行は行わず、スレッドローカルなランタイム設計を維持する。

2. **TLS オフロード (FreeBSD kTLS)**
   - FreeBSD は歴史的にカーネル TLS をいち早く実装した OS である（FreeBSD 13+ で安定）。
   - `ktls` / `rustls` を FreeBSD のソケットオプション (`TCP_TXTLS_ENABLE` / `TCP_RXTLS_ENABLE`) で有効化する処理を追加し、Linux 同様のゼロコピー・暗号化オフロードを実現する。

3. **セキュリティ・サンドボックス (Capsicum)**
   - `seccomp` と `Landlock` の代替として、FreeBSD の **Capsicum** を導入する。
   - `cap_enter(2)` および `cap_rights_limit(2)` を用いて、初期化後にファイルシステムアクセスやネットワークアクセスの権限をドロップし、強固なプロセスの隔離を行う。

4. **ソケット負荷分散 (SO_REUSEPORT_LB)**
   - FreeBSD の `SO_REUSEPORT_LB` ソケットオプションを使用して、複数ワーカー・スレッドへの効率的なコネクション分散を実現する（CBPF の代替）。

5. **ゼロコピー転送 (FreeBSD sendfile)**
   - FreeBSD 独自の強力な `sendfile(2)` システムコール（ヘッダー/トレーラーの同時送信機能を含む）を利用し、静的ファイルの配信を最適化する。

## 改修案（ステップ）

1. `cfg(target_os = "freebsd")` による OS 判定と、Linux 固有モジュール（`seccomp.rs`, `landlock.rs` 等）の分離リファクタリング。
2. FreeBSD 用の代替実装（`capsicum.rs` 等）の作成。
3. `monoio` の kqueue モードでのビルドおよびイベントループ動作検証。
4. FreeBSD 上での kTLS オフロードの検証と実装。
5. FreeBSD 上での `SO_REUSEPORT_LB` と `sendfile` の最適化。

## 受け入れ条件

- FreeBSD (13.x または 14.x) 上で、機能フラグ（例: `--features "freebsd-native"`) を用いてビルドが通る。
- Capsicum による権限ドロップが有効に機能し、許可されていないファイルアクセスがブロックされる。
- kTLS が有効な場合、FreeBSD カーネルレイヤーで TLS 暗号化が行われていることが確認できる。
- E2E テストが FreeBSD 上で Linux と同等にパスする。

## 依存・リスク

- `monoio` が FreeBSD (`kqueue`) 上でどこまで高スループットを出せるかの検証が必要。
- Capsicum の制限下でのログ出力や証明書再読み込み（ホットリロード）の設計見直しが必要になる可能性（権限ドロップ後のファイル記述子の扱い等）。
