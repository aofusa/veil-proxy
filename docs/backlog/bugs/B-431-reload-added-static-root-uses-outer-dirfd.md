# B-431: リロードで足した（起動時に開けなかった）入れ子の File のルートの下を、外側のルートの dirfd で開いて外側の封じ込めで許していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Static roots added on config reload are not registered, so per-route containment falls back to an ancestor root's
dirfd」・「Static-file containment uses an outer route's root for a File root that was never registered」（同じ指摘が
2 件）から確認。

## 事象

- 静的配信の dirfd（Linux の openat2 `RESOLVE_BENEATH`・FreeBSD の capsicum）は起動時に 1 回だけ登録する
  （`cache::resolve::register_static_roots`。`OnceLock`）。リロードで足したルート・起動時に開けなかったルートは
  登録されない。
- ルートを指定しない解決（`open_for_request`・`open_cached(None)`・FreeBSD の `with_resolved_root`）は、パスを
  含む登録済みのルートのうち最も深いものの dirfd で開く。登録されていない内側のルート（`/srv/public`）の下のパスは
  外側のルート（`/srv`）の dirfd で開かれ、内側のルートの外を指す相対シンボリックリンク（`shared -> ../private`）も
  外側の中なので通った。開いたパスは生のパスのまま返り、内側のルートの検査（`sendfile_base_contains`）は生のパスが
  ルートの下なら通すので、外側のルート（IP 制限・JWT 付き）だけが配る場所のファイルが内側の公開のルートから返った
  （B-305 の入れ子のルートの漏れの、登録されていないルートでの再発）。

## 対応

- 設定ロード（起動・リロード）のたびに、設定の File のルートのうち dirfd の表に無いものを記録する
  （`resolve::set_configured_roots`）。ルートを指定しない解決で、選んだ登録済みのルートとパスの間に登録されていない
  ルートがあり、パスがその下なら、外側のルートの dirfd で直には開かない。登録されていないルートが無ければ原子変数の
  読み取り 1 回だけ。
  - Linux: canonicalize の経路（正規化したパスでルートの封じ込めを判定する）へ回す。
  - FreeBSD: 登録されていないルートのうち最も深いものを外側のルートの dirfd の下に開き（`O_RESOLVE_BENEATH`）、
    その下に開く（`security::capsicum` の `resolve_in`。`O_RESOLVE_BENEATH` が内側のルートへ封じ込める）。
    canonicalize の経路へ回すと capability mode では絶対パスを解決できず、F-178 が受け入れるリロード（登録済みの
    ルートの下を指すルートの追加）の後、そのルートの下が外側のルートからの要求も含めて 404 になった（最初の修正で
    FreeBSD もこの経路へ回していて、FreeBSD 14.3 の VM のサンドボックスの E2E で見つけた）。要求ごとに `openat`・
    `close` が 1 本ずつ増えるのは、リロードで足したルートの下だけ（再起動すれば登録され、直に開く）。capability mode の
    外で内側のルートの外を指すシンボリックリンク等が `ENOTCAPABLE` になったら、従来どおりその要求だけ canonicalize の
    経路で判定し直す（`retry_outside_capmode`）。
- 登録されていないルートの集合がリロードで変わったら、外側のルートの dirfd で開いて入れた静的配信のキャッシュ
  （open_file_cache・静的コンテンツキャッシュ）を捨てる。

## テスト

- `cache::resolve::tests::unregistered_nested_root_shadows_outer_dirfd`（入れ子の登録されていないルートは最も深い
  ものを選ぶことも）。
- FreeBSD の `security::capsicum::inner_root_tests::inner_root_confines_paths_below_it`（内側のルートの下は開け、
  内側のルートの外・外側のルートの中を指すシンボリックリンクは断る。外側の dirfd で直に開くと通ってしまうことも確かめる）。
- 自己完結型 E2E `b431_nested_root_added_by_reload_keeps_its_containment`（リロードで足した `/srv/public` の
  `shared -> ../private` を HTTP/1.1・h2c とも返さない）。
- FreeBSD 14.3 の VM のサンドボックスの E2E（`bsd-vm.sh freebsd x86_64 security-e2e`）の capability mode の設定の
  リロード（登録済みのルートの下を指すルートを足すと配れる。`RELOAD n2 http=200`。登録されていないルートの追加は従来どおり
  断る）。同じ VM で単体 1,291・統合 63・自己完結型 E2E 55 が通る。Linux は単体 1,414・統合 63・自己完結型 E2E 58
  （io_uring・epoll）・E2E 595。
