# B-71: OpenBSD x86_64 の E2E で HTTP/3 キャッシュ系 4 件が恒常的に失敗する

**優先度**: P2
**ステータス**: 解消（2026-10-10 の再検証で再現しない。feat/v080-limitations）
**発見日**: 2026-08-22（F-158 のマルチプラットフォーム検証中）
**関連**: B-68（FreeBSD の HTTP/3 E2E 失敗）、B-70（NetBSD の E2E panic）

---

## 事象

OpenBSD 7.9 **x86_64**（QEMU + KVM）の E2E で、次の 4 件が**全実行で必ず失敗**する。

- `test_http3_buffering_spillover`
- `test_http3_cache_hit_miss`
- `test_http3_cache_invalidation`
- `test_http3_cache_stale_if_error`

**OpenBSD aarch64 では同じコードで 543 passed / 0 failed** のため、**x86_64 固有**である。

## F-158 とは無関係であることの実測（ベースライン交互実行）

同一 VM で、F-158 適用前（`b239dc1~1`）と適用後を交互に実行した:

| テスト | ベースライン | F-158 (A) | F-158 (C) | F-158 (D) |
|---|---|---|---|---|
| `http3_buffering_spillover` | ❌ | ❌ | ❌ | ❌ |
| `http3_cache_hit_miss` | ❌ | ❌ | ❌ | ❌ |
| `http3_cache_invalidation` | ❌ | ❌ | ❌ | ❌ |
| `http3_cache_stale_if_error` | ❌ | ❌ | ❌ | ❌ |
| `oversized_request_line` | ❌ | ❌ | ✅ | ✅ |
| `oversized_header` | ❌ | ✅ | ✅ | ✅ |
| `h2c_large_request_body` | ✅ | ❌ | ❌ | ✅ |
| `h2c_invalid_frame` | ✅ | ❌ | ✅ | ❌ |
| 結果 | 537/6 | 536/7 | 538/5 | 538/5 |

**上記 4 件の HTTP/3 テストはベースラインでも同じように失敗する**ため F-158 とは無関係。

## h2c 2 件は「F-158 の回帰」ではない（フレーキー）

`h2c_large_request_body` / `h2c_invalid_frame` は F-158 実行でのみ失敗が観測されたため
一度は回帰を疑ったが、**同一 F-158 バイナリでの実行間で再現しない**:

- run C: `h2c_invalid_frame` は **成功**、`h2c_large_request_body` は失敗
- run D: `h2c_large_request_body` は **成功**、`h2c_invalid_frame` は失敗

**決定的な回帰であれば毎回同じテストが失敗するはず**であり、両テストとも
F-158 実行のいずれかで成功している。`oversized_request_line`/`oversized_header` も
同様に実行ごとにブレており（`oversized_header` はベースラインでのみ失敗）、
**このスイートは OpenBSD x86_64 環境で実行ごとの揺らぎを持つ**。

補強材料として、同じ F-158 コードで次はいずれも h2c が通っている:

- OpenBSD **aarch64**: 543 passed / 0 failed
- Linux `--features epoll`（同じ reactor バックエンド）: 544 / 544
- FreeBSD x86_64: 543 / 1（既知フレーキーの `test_http3_large_request_body` のみ）

## 調査の起点

- 4 件はいずれも **HTTP/3 のキャッシュ／バッファリング**に関わる。
  `http3_cache_*` が 3 件揃って落ちることから、
  QUIC ストリーム上のレスポンスキャッシュ経路の x86_64 OpenBSD 固有の問題を疑う。
- OpenBSD aarch64 で通ることから、アーキ依存（アラインメント・
  `boring`/`ring` のアセンブリ経路）の可能性がある。
- E2E スイート全体の実行ごとの揺らぎ（h2c/oversized 系）は別途、
  QEMU x86_64 上のタイミング感度として切り分けが要る。

## 再検証（2026-10-10、feat/v080-limitations）

同じ OpenBSD 7.9 x86_64（KVM）でフル E2E を実行し、**564 passed / 0 failed / 1 ignored**（ignored は B-58）。
4 件（`http3_buffering_spillover` / `http3_cache_hit_miss` / `http3_cache_invalidation` /
`http3_cache_stale_if_error`）はいずれも成功した。HTTP/3 のバッファ経路（キャッシュ・`buffering = full`）を
メインループで await しないようにした B-97 と、上流接続の再利用（B-104）で、これらの要求が同じワーカーの
他の処理に引きずられなくなったことが効いたと考えられる（単独実行では元々成功していた）。再発したら再オープンする。
