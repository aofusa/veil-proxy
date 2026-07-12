# B-13: seccomp 許可リストに `faccessat2` が無く静的ファイル配信が 404 になる

## 出典

初回の Veil vs nginx 計測（コンテナ環境で wrk (HTTP/1.1) が
全て Non-2xx、musl 版はファイル配信されず）。要約は `docs/perf/history.md` §2 を参照。

## 事象（再現手順）

- `docker/assets/conf.d/config.toml`（`enable_seccomp = true` / `seccomp_mode = "filter"`）で
  コンテナ（`veil:glibc` / `veil:musl`）を起動し、`/`（`File` アクション、静的配信）へ
  HTTP/1.1 でリクエストすると **404 Not Found** が返る。curl / wrk いずれでも再現。
- HTTP/2 (h2load) は 200 で配信される（glibc）。
- ローカル再現: `seccomp_mode = "filter"` の設定で veil を起動し
  `curl --http1.1 https://127.0.0.1:8443/` → 404。`seccomp_mode` を外すと 200。

## 影響

- **P1**。seccomp 有効時に静的ファイル配信（`File` / SendFile 経路）が全滅する。
  コンテナのデフォルト設定が該当するため、既定構成で静的配信が機能しない。
- musl 版はファイル解決系で `faccessat2` を多用するため、HTTP/1.1・HTTP/2 の双方が
  影響を受けファイル配信が成立しなかった。

## 調査メモ

- `strace -f` で veil の syscall を追跡すると、リクエスト処理中に
  `faccessat2(AT_FDCWD, "/var/www/", F_OK, AT_EACCESS) = -1 EPERM` が観測された。
- 原因: glibc 2.33+ / musl は `access()` / `faccessat()` を **`faccessat2`（syscall 439）**
  経由で発行する。veil の seccomp 許可リスト（`src/security.rs` `ALLOWED_SYSCALLS`）には
  `access`(21) はあるが `faccessat`(269) / `faccessat2`(439) が無く、`SECCOMP_RET_ERRNO|EPERM`
  で拒否されていた。ファイル解決（canonicalize/metadata の offload 経路）が EPERM で失敗し
  静的配信が 404 になっていた。
- io_uring 経由の openat/read は seccomp の対象外（IORING_REGISTER_RESTRICTIONS で別管理）の
  ため HTTP/2 の一部経路は素通りしていたが、libc の access チェックが挟まる HTTP/1.1 経路で
  顕在化した。
- 外部（Docker）seccomp プロファイル `docker/assets/security/seccomp.json` にも `faccessat2`
  が無く、二重に拒否していた。

### musl 版の追加要因（open(2)）

`faccessat2` 追加後も **musl 版コンテナ**は 404 のままだった。musl バイナリを host で
`strace` した結果、musl libc の `canonicalize`／ファイルオープンは `openat`(257) ではなく
**レガシー `open`(2)** を発行しており（`open("/var/www/index.html", O_RDONLY|O_LARGEFILE|O_CLOEXEC) = -1 EPERM`）、
許可リストに `open`(2) が無いため拒否されていた（glibc は `openat` を使うため影響なし）。

## 改修内容

- `src/security.rs`: `ALLOWED_SYSCALLS`（x86_64）に `open`(2) を追加（musl 対応）。
  `faccessat2`(439) を x86_64 / aarch64 両方へ、`faccessat`(269) を x86_64 へ追加。
- `docker/assets/security/seccomp.json`: `faccessat` / `faccessat2` を許可リストへ追加
  （`open` は既存）。
- 回帰テスト `security::tests::test_allowed_syscalls_contains_file_access` に
  `open`(2) / `openat`(257) / `faccessat2`(439) の存在確認を追加。

## 受け入れ条件

- [x] `seccomp_mode = "filter"` 構成で HTTP/1.1 / HTTP/2 とも静的ファイルが 200 で配信される。
- [x] `cargo test --lib security::tests::test_allowed_syscalls_contains_file_access` が通過。
- [ ] `veil:glibc` / `veil:musl` を再ビルドしコンテナで 200 配信を確認（同 PR で実施）。

## 関連

- B-14（`cache` feature 無効時の 404、別要因）
- F-25（seccomp BPF 引数レベル検証）

## 対応状況: 完了（コンテナ再ビルド確認のみ残）
