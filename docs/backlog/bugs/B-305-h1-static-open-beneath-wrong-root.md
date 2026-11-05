# B-305: HTTP/1.1 の静的配信だけ、ルート自身ではなく前方一致した最初の登録ルートの dirfd で開く（入れ子のルートで別ルートのファイルを配り得る）

**状態: 修正済み（v0.11.0）**

## 事象（コードの読み取り）

- HTTP/2・HTTP/3 はディレクトリのルートで `open_beneath_in_root`（そのルート自身の dirfd。F-154）を使う。
- HTTP/1.1 は `get_file_info_with_config` → `open_beneath_for_request`（`src/cache/resolve.rs:538-566`）で、**パスが前方一致した最初の登録ルート**の dirfd を使う。
  その後の `sendfile_base_contains` は生のパス同士の比較なので常に真（`file_cache.rs:376-384` のコメントどおり）。
- 登録ルートが入れ子（`/srv` と `/srv/public`）で `/srv` が先に登録されていると、`/srv/public` のルート配下の相対シンボリックリンク（`link -> ../private/x`）が
  `/srv` の封じ込めで許され、別のルート（認証・IP 制限付き）配下のファイルを HTTP/1.1 でだけ配れる。

## 改修案

- HTTP/1.1 もルート自身の dirfd（`open_beneath_in_root`）へ寄せ、3 プロトコルで同じ関数にする。[F-294](../features/F-294-precompressed-static-siblings.md) の兄弟の探索も最初からルート単位にする。
- 実機確認: 入れ子の 2 つの File ルート（外側に `allowed_ips`）と内側の相対リンクで、HTTP/1.1・HTTP/2・HTTP/3 の結果を比べる。確認できたら優先度を上げる。
  回帰試験（統合試験）にする。Linux（openat2）・FreeBSD（capsicum）・その他（canonicalize の経路）。

## 修正

- 登録済みの静的ルート（Linux の `LINUX_ROOTS`・FreeBSD の capsicum の `STATIC_DIRS`）を深い順に並べ、HTTP/1.1 の「パスが前方一致したルート」探索が入れ子の内側のルートの dirfd を使う。内側のルートの相対シンボリックリンクで外側（別のルート）へ出られない。
- 外側のルートの要求で内側のディレクトリを通るパスも内側の封じ込めになる（より厳しくなるだけ）。HTTP/2・HTTP/3 は従来どおり経路自身の dirfd。
- 試験: `cache::static_file::tests::nested_root_link_cannot_escape_through_outer_root`。

## 関連

- [F-153](../features/F-153-static-open-resolve-beneath.md)（openat2・O_RESOLVE_BENEATH）、[F-294](../features/F-294-precompressed-static-siblings.md)
