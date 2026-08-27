# B-79: 依存する wasmtime 40.0.4 に RUSTSEC 勧告（うち適用対象 2 件）

## 事象

`tools/container_security` の `cargo_audit` フェーズが 16 件の勧告で失敗する。
うち 12 件は **wasmtime 40.0.4**（`wasm` feature 有効時のみ依存）由来。

## トリアージ（実際に veil へ適用されるか）

### 適用対象外: 別の依存ツリー（配布バイナリに入らない）

| 勧告 | クレート | 判定 |
|---|---|---|
| RUSTSEC-2026-0258 | `h2` 0.4.15 | **dev 依存のみ**。`cargo tree -e normal --features full -i h2` が空。veil は HTTP/2 を自前実装しており `h2` クレートを使わない |
| RUSTSEC-2026-0185 | `quinn-proto` 0.11.14 | **dev 依存のみ**（`tests/common/http3_client.rs` / `tools/perf/h3load`）。本番データプレーンは quiche（AGENTS.md の方針どおり） |

### 適用対象外: 当該機能を使っていない

| 勧告 | 内容 | 判定 |
|---|---|---|
| RUSTSEC-2026-0095（**critical 9.0**） | Winch backend でサンドボックス脱出 | **veil は Winch を使わない**（`src/wasm/registry.rs` は Cranelift、OpenBSD/NetBSD 等は Pulley）。`grep -rn winch src/ Cargo.toml` は 0 件 |
| RUSTSEC-2026-0089 / 0086 / 0094 | Winch backend の panic / データ漏洩 / 戻り値マスク | 同上 |
| RUSTSEC-2026-0092 / 0093 / 0091 / 0085 | component model の文字列トランスコード・`flags` lifting | **Proxy-Wasm はコア wasm モジュールのみ**を読み込み、component model API を使わない |
| RUSTSEC-2026-0114 / 0222 | テーブル確保時の panic / エンジン間の型インデックス混同 | 単一エンジン・通常のテーブルサイズのため到達しない |

### 適用対象（要対応）

| 勧告 | 深刻度 | 内容 | なぜ適用されるか |
|---|---|---|---|
| **RUSTSEC-2026-0096** | **critical 9.0** | aarch64 Cranelift のゲストヒープアクセス誤コンパイルでサンドボックス脱出 | veil は **aarch64（Linux / macOS）で Cranelift を使う** |
| **RUSTSEC-2026-0088** | low 2.3 | プーリングアロケータのインスタンス間データ漏洩 | veil は `InstanceAllocationStrategy::Pooling` を使う（OpenBSD の OnDemand 強制を除く） |

**緩和要因（脅威モデル）**: Proxy-Wasm モジュールは **運用者が設定で指定する**もので、
攻撃者が任意に持ち込めるものではない。悪意ある .wasm を意図的にロードしない限り
サンドボックス脱出は成立しない。また `wasm` は **デフォルト feature ではない**
（`default = ["ktls", "http2", "mimalloc"]`）。

## 修正できない理由（本チケットを分離した理由）

**40.0.4 が 40.x 系の最終リリース**であり、パッチ版が存在しない
（`cargo info wasmtime` → `version: 40.0.4 (latest 47.0.3)`）。
解消には **47.x への メジャー 7 段階のアップグレード**が必要で、さらに

- `third_party/wasmtime`（B-55 の vendoring 版 `veil-wasmtime`。NetBSD 全アーキ・
  FreeBSD/OpenBSD aarch64 が使う）の **再 vendoring**（`README.veil.md` の手順）
- wasmtime 40→47 の API 変更への追従（`src/wasm/` 全体）
- 6 プラットフォーム × 各アーキの再検証

を伴う。**本セッションのベンチマーク結果を無効化する規模**のため、独立したタスクとして分離する。

## 対応済み（本セッション）

同時に検出された、**すぐ直せる**勧告は解消した。

| クレート | 変更 | 勧告 |
|---|---|---|
| `anyhow` | 1.0.102 → 1.0.104 | RUSTSEC-2026-0190（unsound: `Error::downcast_mut()`） |
| `crossbeam-epoch` | 0.9.18 → 0.9.20 | RUSTSEC-2026-0204（`fmt::Pointer` の不正ポインタ参照） |
| `lru` | 0.16.2 → 0.18.2 | RUSTSEC-2026-0253（`LruCache::pop()` の panic 安全性） |

`lru` は semver メジャー相当の更新だが **API 変更なしでコンパイルが通り**、
clippy・単体 955・統合 54・E2E 544 すべて pass することを確認した
（veil は `LruCache::pop()` を呼んでいないため元々到達しないが、更新できるものは更新した）。

`bitmaps` / `im-rc` / `sized-chunks` の unmaintained 警告（RUSTSEC-2026-0247 / 0250 / 0251）と
`sized-chunks` の unsoundness（RUSTSEC-2026-0255）は、いずれも
**wasmtime → wasm-compose → im-rc** 経由の間接依存であり、wasmtime のアップグレードで解消する。

## 次にやること

wasmtime 47.x へのアップグレードと `third_party/wasmtime` の再 vendoring を
独立タスクとして実施する。実施までは本チケットが既知の未対応事項として残る。
