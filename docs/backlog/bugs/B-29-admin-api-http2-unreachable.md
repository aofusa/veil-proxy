# B-29: 管理 API (/__admin) が HTTP/2 経路で到達不能（404）

## 事象

HTTPS + HTTP/2（curl 既定 ALPN）で `GET /__admin/config` を送ると **404** が返る。
同一リクエストを `--http1.1` にすると **401**（未認証）となり、管理 API が正しく動作する。

container_security の `admin_security_probe` 初回実行で検出。

## 再現

```bash
curl -sk -o /dev/null -w "%{http_code}\n" https://<veil>:443/__admin/config        # 404
curl -sk --http1.1 -o /dev/null -w "%{http_code}\n" https://<veil>:443/__admin/config  # 401
```

## 影響

- HTTP/2 のみを使うクライアント・監視ツールから管理 API にアクセスできない。
- 404 のため「認証失敗」との区別がつかず、運用時のトラブルシュートが困難。

## 調査メモ

- 管理 API 処理は `src/proxy.rs` の HTTP/1.1 リクエスト処理ブロック内にのみ存在。
- HTTP/2 リクエストハンドラには同等の admin 分岐がない。

## 改修案

- HTTP/2 ストリーム処理経路にも `admin_config` の認証・エンドポイント分岐を移植する。
- または HTTP/2 で admin パスを明示的に 421/505 で拒否しドキュメント化（非推奨）。

## 関連

- F-90 admin_security_probe（テストは HTTP/1.1 で検証）
- F-21 管理 Admin API

## 対応状況（完了）

`handle_http2_admin_request` を HTTP/2 単一リクエスト経路へ配線。F-90 `admin_security_probe` 全ケース通過（2026-07-08）。