# F-110: F-109 追加 E2E / プローブの CI 組み込み

## 概要

[F-109](F-109-http3-grpc-parity-coverage.md) で追加した E2E 9 件および container_security プローブ拡張の **CI/CD 配線のみ** を対象とする。

## 背景

F-109 実装本体と CI を分離し、ハーネス・テストのマージを先行させる。**本作業（F-109）では対象外**。

## 改修内容（予定）

1. GitHub Actions の E2E マトリクスに F-109 新規テストが含まれることを確認（`full` feature で `cargo test --test e2e_tests`）
2. `container-security-nightly.yml` で smuggling / websocket / compression / rate_limit / wasm / grpc_web の H3 拡張パスが実行されること
3. 必要なら Job Summary に新プローブ ID を列挙

## 受け入れ条件

- [ ] F-109 追加テストが CI で自動実行される
- [ ] container_security nightly で H3 拡張プローブが走る
- [ ] ドキュメント（README / workflow コメント）更新

## 対応状況

未着手。
