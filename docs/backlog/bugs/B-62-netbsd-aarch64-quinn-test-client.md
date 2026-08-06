# B-62: NetBSD/aarch64 実機で E2E テストクライアントの quinn が panic → プロセス abort し、フル E2E スイートが完走しない

**状態: 未対応（回避策あり）**

## 事象

NetBSD 10.1 evbarm-aarch64（実機、QEMU/HVF on Apple Silicon）で E2E テストクレートを
実行すると、HTTP/3 クライアントとして使っている dev-dependency の `quinn` 0.11.9 /
`quinn-udp` 0.5.14 が `quinn-udp-0.5.14/src/cmsg/mod.rs:86` で panic する。

panic 後、`quinn::Endpoint` の `Drop` 実装内で `PoisonError` を `unwrap()` しており、
**デストラクタ内で二重 panic → プロセス abort（SIGABRT）**になる。この結果、
**E2E テストバイナリ全体が途中で落ちる**。

`--skip common::http3_client --skip http3` のように HTTP/3 系テストを明示的に
スキップしても、他のテスト経由で `quinn::Endpoint` が作られるケースが残るため
**回避しきれなかった**。

## veil 本体の問題ではない

これは **テストハーネス側の HTTP/3 クライアントライブラリ（quinn-udp）**の
NetBSD/aarch64 非互換であり、veil 本体（プロキシ実装）の問題ではない。

- NetBSD **x86_64** では同じテストクレートが完走する（507 passed / 35 failed。
  35 件は全て B-60 の PaX MPROTECT 起因の WASM 系テストで、B-60 対応後は解消する）。
- つまり NetBSD/aarch64 でのみ発生する quinn-udp 側の問題。

## 影響

NetBSD/aarch64 では**フル E2E スイートを完走できない**。

回避策として `TEST_FILTER=wasm_tests` のように quinn（HTTP/3 クライアント）を
使わないテスト群へ絞り込めば完走する（実測: `test result: ok. 23 passed; 0 failed;
519 filtered out`）。

## 本筋の対応

- quinn-udp の NetBSD/aarch64 対応を upstream へ報告・PR する。
- あるいは E2E テストクレートの HTTP/3 クライアントを quiche ベース等（quinn-udp に
  依存しない実装）へ置き換える。

## 関連

- B-59（NetBSD/aarch64 の BoringSSL リンクエラー。本チケットの発見は同じ実機検証で得た）
- B-60（NetBSD の PaX MPROTECT による WASM 実行不能。x86_64 で残る 35 件の fail の原因）
- F-140（NetBSD 対応。BSD 系プラットフォームの実機検証プロジェクトの一環）
