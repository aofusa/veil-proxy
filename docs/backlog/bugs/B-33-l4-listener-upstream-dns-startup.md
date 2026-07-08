# B-33: L4 リスナーが上流 DNS 未解決時に起動失敗する

## 事象

`fixtures/veil-config.toml` の L4 上流が `veil-sec-toxiproxy`（Toxiproxy 未起動時は DNS 失敗）の場合、
起動ログに以下が出て **4443 が待受しない**:

```
[L4:l4-passthrough] failed to parse upstream addresses: ... Temporary failure in name resolution
```

`l4_flood_probe` では `l4_connections_opened: 0/80` となるが、HTTP/443 は生存のためプローブは合格。

## 影響

- カオス基盤（Toxiproxy）起動前や Landlock 下でホスト名が解決できないと L4 機能全体が無効化。
- L4 セキュリティテストが実質スキップされる。

## 改修案

- 起動時は上流アドレス解決を遅延し、接続時に解決・リトライ（HTTP Proxy 上流と同様の IP 置換とも整合）。
- または設定検証で起動前に解決可能なアドレスのみ受け付ける。

## 関連

- F-90 l4_flood_probe
- F-18 L4 ストリームプロキシ

## 対応状況（完了）

`L4UpstreamTarget` + `resolve_upstream_target`（offload 遅延 DNS 解決）を実装。F-90 `l4_flood_probe` で 80/80 接続成功（2026-07-08）。