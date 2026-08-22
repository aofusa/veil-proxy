# B-70: NetBSD の E2E が `test_http3_client_creation` の panic で中断する

**優先度**: P2
**ステータス**: 未修正（F-158 とは無関係であることを構造的に確認済み）
**発見日**: 2026-08-22（F-158 のマルチプラットフォーム検証中）
**関連**: B-51（OpenBSD の aws-lc-rs SIGSEGV）、F-140（NetBSD の暗号プロバイダ選択）

---

## 事象

NetBSD 10.1 aarch64 で `tools/qemu/bsd-vm.sh netbsd aarch64 e2e` を実行すると、
テストバイナリが panic で中断する。

```
thread 'common::http3_client::tests::test_http3_client_creation' (17623)
  panicked at library/core/src/panicking.rs:233:5:
error: test failed, to rerun pass `--test e2e_tests`
```

panic により**テストバイナリ全体が中断するため `test result` 行が出力されず、
他のテストの合否が一切わからない**。

## 該当テスト

`tests/common/http3_client.rs:413`

```rust
async fn test_http3_client_creation() {
    let result = Http3TestClient::connect("127.0.0.1:8443".parse().unwrap(), "localhost").await;
    assert!(result.is_err() || result.is_ok());   // 常に真
}
```

アサーション自体は恒真なので、**panic は `Http3TestClient::connect` の内部**
（quinn の QUIC エンドポイント生成、または rustls 暗号プロバイダの導入）で起きている。

## F-158 とは無関係であることの根拠

1. `tests/common/http3_client.rs` は **quinn ベースのテスト用クライアント**であり、
   veil のサーバ側コード（F-158 が変更した `proxy.rs` の h2 spawner・
   `runtime/reactor/executor.rs` の `spawn_inline`）を**一切通らない**。
2. F-158 はこのファイルを変更していない
   （直近の変更は `a608673`（F-140）/ `24e43f0`（B-51）で、いずれも F-158 より前）。
3. **NetBSD の dist ビルド自体は成功している**（3 分 24 秒、`paxctl +m` 適用済み）。
   パッケージ（`veil-0.6.0-aarch64-unknown-netbsd.tar.gz`）も生成済み。
4. 同じテストクライアントを使う **OpenBSD aarch64 の E2E は 543 passed / 0 failed** で
   全通過する（OpenBSD も NetBSD と同じ `ring` プロバイダ構成）。
   したがって「BSD 全般」ではなく **NetBSD 固有**である。

## 調査の起点

- `Http3TestClient::connect` 内の `unwrap()`/`expect()` を特定する
  （panic 位置が `core/src/panicking.rs` なので `Option::unwrap` か `Result::unwrap`）。
- NetBSD で quinn が UDP ソケットを生成できているか
  （`quinn::Endpoint::client` のソケットオプション設定が NetBSD で失敗する可能性）。
- B-51（OpenBSD で aws-lc-rs が SIGSEGV）と F-140（NetBSD の provider を ring に）の
  経緯から、暗号プロバイダ導入まわりの OS 差を疑う。

## 暫定の影響

**NetBSD は「ビルド・パッケージは可能だが、E2E の全体結果が取得できない」状態。**
panic が 1 件でテストバイナリごと落ちるため、他 543 件の合否が不明である。
切り分けには当該テストを `#[ignore]` するか `--skip` で除外して再実行する必要がある。
