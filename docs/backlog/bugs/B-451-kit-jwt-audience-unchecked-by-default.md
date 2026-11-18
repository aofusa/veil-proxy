# B-451: kit の JWT の検証が、受け手（audience）を設定しないと `aud` を見ず、別のサービス向けのトークンを受けていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「JwtVerifier never checks `aud` unless an audience is configured, so tokens issued for another service are accepted
by default (Rust kit and C kit; the Python kit rejects this since B-201)」から確認。

## 事象

- `veil_kit::auth::jwt::JwtVerifier::verify`（Rust の kit。C の helpers の `veil_kit_jwt_verify`・
  `veil_kit_jwt_authenticate` も同じ）は、`audience` を設定したときだけ `aud` を比べていた。`new`・`from_jwks`・
  `veil_kit_jwt_verifier_new` の既定は設定なし。
- 同じ発行者・鍵で複数の受け手へトークンを出す ID プロバイダの下では、別のサービス（攻撃者が自分で登録した
  クライアント、権限を持っている別の API）向けの正しく署名されたトークンを送ると、署名・`exp`・`iss` が通り、そのトークンの
  `sub`・スコープ・ロールで認証された（RFC 7519 §4.1.3 に反する）。Python の kit は B-201 で PyJWT と同じく断っている。

## 対応

- 受け手を設定していなければ、`aud` の付いたトークンを `BadAudience` で断る（PyJWT・Python の kit と同じ）。
  受け手を設定したときは従来どおり `aud` に含むこと。
- 検査を外す明示の設定を足した: Rust は `JwtVerifier::verify_audience(false)`、C は
  `veil_kit_jwt_verifier_verify_audience(v, 0)`（`veil_kit.h`）。
- 利用者ガイド（`library-kit.md` の英日）に書いた。

## テスト

- `veil_kit::auth::jwt::tests::hmac_roundtrip_and_checks`（受け手の設定なしで `aud` 付きは `BadAudience`、
  `verify_audience(false)` で通る、`aud` の無いトークンは通る、受け手を設定して `aud` が無ければ `BadAudience`）。
