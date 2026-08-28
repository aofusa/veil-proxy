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

---

## 2026-08-28 追記: 失敗するテストの顔ぶれが入れ替わり、`test_http3_large_request_body` は「稀なフレーキー」ではなく**恒常失敗**になっている

F-169 / B-76 / B-77 / B-80 の検証で FreeBSD 14.3 aarch64 の E2E を回した結果、
**本チケットが「恒常」としていた 2 件は両方とも成功し、代わりに別のテストが失敗する**
という状態になっていた。

| テスト | 本チケット記載（2026-08-18） | 2026-08-28 実測 |
|---|---|---|
| `test_http3_quic_keepalive_idle` | **恒常失敗**（3/3） | **成功** |
| `test_http3_udp_unreachable_fallback` | **恒常失敗**（3/3） | **成功** |
| `test_http3_large_request_body` | 稀に失敗・**単独実行は 3/3 成功** | **恒常失敗**（単独実行でも失敗） |
| `test_http3_request_body_streaming_tls_backend` | 記載なし | VM 多重時のみ失敗（単独なら成功） |

### `test_http3_large_request_body` は本セッションの変更が原因ではない（A/B で確定）

同一 VM で **ベースライン（`2b0664d` = 本セッション開始時点）と HEAD を交互に実行**した。

| ビルド | 結果 |
|---|---|
| ベースライン `2b0664d` | **FAILED**（0 passed / 1 failed、60.00s タイムアウト） |
| HEAD（F-169 / B-76 / B-77 / B-80 適用後） | **FAILED**（0 passed / 1 failed、60.01s タイムアウト） |

**ベースラインでも同じように失敗する**ため、本セッションの変更（圧縮結果キャッシュ・
CBPF 修正・HTTP/3 静的圧縮・ハーネスの tls_only）とは無関係である。
構造的にも、この経路（HTTP/3 の大きなリクエストボディを TLS バックエンドへ
ストリーミングする F-44 の経路 = `src/http3_stream.rs`）は本セッションで 1 行も変更していない
（B-77 が触ったのは `http3_server.rs` の File/SendFile/MemoryFile の**レスポンス**分岐のみ）。

### 除外した仮説

| 仮説 | 検証 | 結果 |
|---|---|---|
| VM 多重実行による CPU 競合 | 他 2 VM を落として再実行（load 5.28 → 2.31） | 失敗 2 件 → 1 件に減ったが**本件は残る** |
| ゲストのディスク逼迫 | 99%（残 190MB）→ 80%（残 5.0GB）にして再実行 | **変わらず失敗** |
| 上流 `127.0.0.1:19998` が unhealthy | 意図的に到達不能な gRPC フェイルオーバー用上流 | **無関係** |
| 本セッションの変更 | ベースラインとの A/B | **無関係**（上記） |

### 観測されている症状

サーバ側ログ（`/tmp/proxy.log`）に、**30 秒間隔**（= `Read Timeout: 30s`）で

```
WARN [src/http3_stream.rs:763] [HTTP/3] streaming backend read error:
     Connection reset by peer (os error 54)
```

が出る。テストは 4 回リトライするが 1 回あたり約 30 秒を消費するため、
60 秒のテストタイムアウトに到達して失敗する。

**Linux では同じコード・同じ `Cargo.lock` で 544/544 成功する**ため、FreeBSD 固有である。

### 次にやること

- `src/http3_stream.rs` の TLS バックエンドストリーミング経路（F-44）で、
  FreeBSD 上の 1.5MB アップロードがバックエンドから `ECONNRESET` を受ける理由を切り分ける
  （バックエンドフィクスチャ側の受信上限か、veil の chunked 転送側か）。
- 本チケットの「恒常/フレーキー」表は 2026-08-18 時点の観測であり、
  **現在の顔ぶれと一致しない**。回帰の起点は 2026-08-22（B-70 記録時）以降 2026-08-28 までの
  間に入った変更（F-163〜F-168 を含む）である可能性が高く、範囲を二分探索する必要がある。
  **FreeBSD の E2E は 08-22 以降回されていなかった**ため、この間の変更が未検証のまま積み上がっている。
