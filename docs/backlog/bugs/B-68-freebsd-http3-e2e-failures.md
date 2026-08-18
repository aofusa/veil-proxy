# B-68: FreeBSD の HTTP/3 E2E が 2 件恒常的に失敗する（+ 1 件フレーキー）

**優先度**: P2
**ステータス**: 未修正（**F-158 以前から存在することを実測で確認済み**）
**発見日**: 2026-08-18（F-158 の FreeBSD E2E 検証中）

---

## 事象

FreeBSD 14.3 aarch64（QEMU/HVF）で `VEIL_E2E_FEATURES=full-freebsd
VEIL_E2E_NO_DEFAULT=1 bash tests/e2e_setup.sh test` を実行すると、
HTTP/3 のテストが恒常的に 2 件失敗する。

| テスト | 恒常/フレーキー |
|---|---|
| `test_http3_quic_keepalive_idle` | **恒常**（3/3 回失敗） |
| `test_http3_udp_unreachable_fallback` | **恒常**（3/3 回失敗） |
| `test_http3_wasm_local_response` | フレーキー（2 回中 1 回失敗） |

## F-158 とは無関係であることの実測

**同一 VM・同一セッションでベースライン（F-158 適用前）と新コードを交互に実行**して確認した:

| ビルド | 結果 | 失敗テスト |
|---|---|---|
| ベースライン（F-158 前） | 542 passed / **2 failed** | keepalive_idle, udp_unreachable_fallback |
| F-158 適用後 run 1 | 542 passed / **2 failed** | **ベースラインと完全に同一** |
| F-158 適用後 run 2 | 541 passed / 3 failed | 上記 2 件 + wasm_local_response |

**恒常失敗の 2 件はベースラインでも同じように失敗する**ため、F-158 の影響ではない。

加えて F-158 の変更点（`TaskPool::spawn_inline`）の呼び出し元は
`proxy.rs` の `h2_task_spawner` **のみ**であり、これは HTTP/2 コネクション処理専用である。
`src/http3_server.rs` は `TaskPool` を一切使っていないため、
**HTTP/3 経路は F-158 の変更コードを 1 行も実行しない**（構造的にも無関係）。

## 調査の起点

- `test_http3_quic_keepalive_idle`: アイドル時のキープアライブ（PING/idle timeout）。
  quiche のタイマー処理と F-151 のイベント駆動ループ（ダーティ接続集合・最小ヒープ）の
  相互作用を疑う。**FreeBSD の reactor バックエンド固有**の可能性がある
  （Linux io_uring / epoll では 544/544 で全通過するため）。
- `test_http3_udp_unreachable_fallback`: UDP 到達不能時のフォールバック。
  ICMP port unreachable の扱いが Linux と FreeBSD で異なる可能性。
- `test_http3_wasm_local_response`: フレーキー。負荷/タイミング依存と思われる。

## 注意

Linux では **io_uring・epoll とも E2E 544/544 で全通過**する
（`test_http3_large_request_body` が稀に失敗するが、実行ごとにバックエンドをまたいで
出現位置が入れ替わるため環境フレーキーと確定済み。単独実行では 3/3 成功）。
本件は **FreeBSD 固有**である。
