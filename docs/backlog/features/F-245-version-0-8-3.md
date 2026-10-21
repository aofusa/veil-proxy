# F-245: v0.8.3 のリリース（ライブラリの使い勝手と英語化）

**状態: 設計（feat/v083-release、2026-10-21）**

## 目的

F-241〜F-244 を 1 つのリリース（v0.8.3）として、crates.io・PyPI・ghcr.io に公開する。

| チケット | 内容 | 利用者から見える変更 |
|---|---|---|
| [F-241](F-241-docsrs-english-only.md) | docs.rs を英語だけにし、チケット番号と経緯を出さない | docs.rs の文書 |
| [F-242](F-242-english-only-log-messages.md) | ログ・エラー・panic の文を英語だけにし、チケット番号を出さない | ログの文（キーと形式は不変） |
| [F-243](F-243-library-without-cargo-config-env.md) | `.cargo/config.toml` の `[env]` 無しで Linux の `http3` を使える（無ければ quiche 同梱の BoringSSL） | 依存の書き方が `Cargo.toml` だけで済む |
| [F-244](F-244-library-default-access-log.md) | ライブラリの既定でアクセスログを stdout に出す | ライブラリの既定の設定 |

## 版

- 0.8.2 → **0.8.3**（パッチ）。
  - 公開 API は変えない。
  - ライブラリの既定の設定（F-244）とログの文（F-242）は振る舞いの変更だが、互換を壊さない。前者は設定 1 行で元に戻せる。
- `veil-proxy-quiche` は `0.24.9-veil.1` → `0.24.9-veil.2`（F-243）。`veil-proxy-wasmtime` は変えない。

## 順番

1. F-242（ログ）と F-241（docs.rs）
   - 文字列と doc の変更だけなので先に入れる。検査の台本（`check-messages.py`・`check-docs.sh`）もここで CI に入れる。
2. F-244（アクセスログ）
   - 既定の設定の変更と試験。API のベンチの設定を先に直し、条件を揃えてから既定を変える。
3. F-243（BoringSSL の同梱）
   - いちばん大きい。段階 0（外部のプロジェクトで両方式のリンクを確かめる）が通らなければ、方式から見直す。
   - crate の doc の「HTTP/3 on Linux」の節は、この変更で書き直す（F-241 の規則で）。
4. `tools/release/bump-version.sh 0.8.3`。`main` へマージし、rc のタグ（`v0.8.3-rc.1`）で workflow を最後まで通す。
   - crates: dry-run。F-243 の `.crate` から `[env]` 無しでビルドする確認を含む。
   - PyPI: TestPyPI へ送る。
   - イメージ: 4 アーキの試験。
5. `v0.8.3` のタグで公開する。

## 公開の後の確認

- crates.io の `veil-proxy`・`-macros`・`-build` が 0.8.3、`veil-proxy-quiche` が `0.24.9-veil.2`。
  - `.crate` の `.cargo_vcs_info.json` が、タグのコミットを指す。
- docs.rs の 0.8.3 の 3 つのページが英語だけで、チケット番号が無い（F-241 の受け入れ条件）。
- **crates.io から入れる新しいプロジェクト**（`.cargo/config.toml` 無し）で、`veil-proxy = "0.8.3"`（`full`・`api`）がビルドでき、
  HTTP/3 に応答し、アクセスログが stdout に出る（F-243・F-244）。
- PyPI の `veil-api` 0.8.3 の wheel で、アクセスログが既定で出る。
- ghcr.io の `latest`・`musl`・`glibc` と `*-v0.8.3` の 2 つの系列が `veil 0.8.3` を返す。
  - イメージは veil のリポジトリでのビルド（`[env]` あり）なので、BoringSSL を含まない。起動ログの `HTTP/3: TLS backend:` で確かめる。
- `docs/artifacts/v0.8.3_release_note.md` を書く。

## 受け入れ条件

- F-241〜F-244 の受け入れ条件をすべて満たす。
- `v0.8.3-rc.1` と `v0.8.3` の release workflow が成功する。
- 上の「公開の後の確認」がすべて通る。

## 関連

- [F-231](F-231-version-0-8-0.md)（版の上げ方）
- [F-237](F-237-publish-container-images.md)・[F-238](F-238-publish-crates-and-pypi.md)（公開の workflow）
- `packaging/README.md` の「公開」
