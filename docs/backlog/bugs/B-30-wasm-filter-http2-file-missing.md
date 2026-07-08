# B-30: WASM フィルタが HTTP/2 File 応答に適用されない

## 事象

`/wasm/` ルートに `header_filter` モジュールを適用した File 配信で:

- HTTP/1.1: `X-Veil-Processed: true` が付与される
- HTTP/2: 200 だが **WASM 由来ヘッダが一切付かない**

## 再現

```bash
curl -sk --http1.1 -D - -o /dev/null https://<veil>/wasm/ | grep -i veil-processed  # あり
curl -sk -D - -o /dev/null https://<veil>/wasm/ | grep -i veil-processed               # なし
```

## 影響

- HTTP/2 クライアント（ブラウザ・多くの SDK）では Proxy-Wasm フィルタが事実上無効化される。
- セキュリティポリシー（WAF 等）を WASM で実装した場合に抜け道となる。

## 改修案

- HTTP/2 File/Proxy 応答生成経路に WASM フィルタ呼び出しを配線（HTTP/1.1 と同等の lifecycle）。

## 関連

- B-04, B-05（WASM 適用経路の過去バグ）
- F-90 wasm_security_probe（HTTP/1.1 で検証）