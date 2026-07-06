# F-67: バックエンドのプロトコル違反テスト

出典: `security_chaos_fuzzing_report.md` §2.2.3。親: [F-53](F-53-chaos-engineering-expansion.md)。

## 目的

正常な HTTP 応答を返さないバックエンド（ヘッダー途中切断・Content-Length 不一致・
巨大ヘッダー・不正ステータス・無応答）に対し、Veil が安全に処理
（速やかな 502/接続クローズ）し、クラッシュ・ハング・レスポンススマグリングを
起こさないことを検証する。

## 実装済み

- モックバックエンド `tools/container_security/harness/scripts/bad_backend_server.py`
  （標準ライブラリのみ。パス別に 7 種の不正応答を返す）。
- カオススクリプト `tools/container_security/chaos/bad_backend_chaos.sh`
  （専用 Veil + バックエンドを起動、8 プローブ、panic/OOM 痕跡を検査。既定 `SKIP_BAD_BACKEND=1`）。
- サンドボックス下の実行時 DNS 制約に対応し、上流は IP 直指定
  （既存 `prepare_veil_test_config` と同方針）。

## 実行結果（顕在化した問題は修正せず backlog 化）

- 正常系 `/ok` = 200、Content-Length 過小 `/cl-too-small` = 200（スマグリング無し）で問題なし。
- **[B-16](../bugs/B-16-splice-pipe-refcell-borrow-panic.md)**: 並行アクセス時に
  `src/pool.rs:401`（kTLS splice パイプ取得）で **RefCell 二重借用 panic**。
- **[B-17](../bugs/B-17-malformed-backend-client-hang.md)**: ヘッダー異常・早期切断・
  巨大ヘッダー時に **クライアント可視のハング**（速やかな 502/クローズにならない）。

## 実装済み（HTTP/2 上流耐性・2026-07-06）

- **H2C クライアント（proxy→h2 バックエンド）の違反応答耐性を単体テスト化**
  （`src/http2/client.rs` の `backend_*` 5 件）。不正な h2 バックエンド応答に対し
  クライアントが **panic・無限ループせず必ず `Err` を返す**ことを検証:
  - 即時 EOF（無応答切断）→ `ConnectionClosed`
  - 切り詰めフレーム（宣言長にペイロード未達で EOF）→ `Err`
  - フレーム解釈不能なゴミバイト列 → panic せず `Err`
  - バックエンド `GOAWAY` → `ConnectionClosed`
  - 対象ストリームへの `RST_STREAM` → stream error `Err`
- ScriptedStream モック + 同期ドライバで、実バックエンド不要にホワイトボックス検証。

## 残件

- HTTP/3 上流に対する同種の違反注入（quiche クライアント経路）。
- コンテナレベルの h2 フレーム単位モックバックエンド（現状は HTTP/1.1 の
  `bad_backend_server.py` + h2 クライアント単体テスト）。
- B-16 / B-17 修正済み。`bad_backend_chaos.sh` の CI 化は [F-57](F-57-container-security-ci.md) 管理。

## 受け入れ条件

- `SKIP_BAD_BACKEND=0 ./tools/container_security/run.sh` が各プローブを実行し、
  修正後は全プローブがタイムアウトせず 502/クローズを返し、panic 痕跡が無いこと。
