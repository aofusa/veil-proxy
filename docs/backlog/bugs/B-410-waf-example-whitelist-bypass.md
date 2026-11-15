# B-410: 例の WAF の許可リストを、偽装したヘッダ・CIDR の誤った照合・パスの前方一致で素通りできた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

## 事象

B-406（WAF の例のフィルタ）を直す途中で見つけた。`examples/wasm-filters/waf-filter` の許可リスト（検査を飛ばす条件）に 3 つの穴が
あった。例は利用者がそのまま配備し得る WAF として README で案内している。

1. `whitelist_ips` を、クライアントが自由に書ける `X-Forwarded-For`・`X-Real-IP`・`X-Client-IP` で照合していた。許可した
   アドレスを `X-Forwarded-For` に書けば、誰でもすべての検査を飛ばせた。
2. CIDR の照合は `/16`・`/24`・`/32` だけを扱い、それ以外の長さは文字列の前方一致だった。`1.2.3.4/30` は `1.2.3.40`〜`1.2.3.49`
   も許し（広すぎる）、`10.0.0.0/8` は何も許さなかった。アドレスからポートを外すのに `:` で切っていたので IPv6 は一致しなかった。
3. `whitelist_paths` を `:path`（クエリ文字列を含む）の前方一致で照合していた。`/health` は `/healthz`・`/health-admin`・
   `/health/../admin`・`/health%2f..%2fadmin` も許した（上流が正規化・復号すると別のパスになる）。

## 修正

- `whitelist_ips` はプロパティ `source.address`（veil が接続から得た相手のアドレス）だけで照合する。モジュールの
  `allowed_properties` に `source.address` が要る（無ければ許可リストは効かず、検査する）。
- アドレス・CIDR は `std::net` で解析し、IPv4・IPv6 とも接頭辞の長さのマスクで照合する（`ip`・`ip:port`・`[v6]:port`、
  IPv4 射影の IPv6 は IPv4 として扱う）。解析できない項目は何にも一致しない。
- `whitelist_paths` はクエリ文字列を除いたパスが、項目そのものかその下（区切りが `/`）のときだけ一致する。`A-Z a-z 0-9 / - . _ ~`
  以外の文字（`%` の符号化・`;`・`\`）や `.`・`..` の区切りを含むパスは許可しない。
- README の設定の表・動作の説明・設定例（`allowed_properties = ["source.address"]`）を直した。

## 試験

例の単体 `test_ip_whitelist_cidr_and_ipv6`・`test_path_whitelist_segment_boundary`（どちらも直す前の実装では失敗する入力を含む）。
