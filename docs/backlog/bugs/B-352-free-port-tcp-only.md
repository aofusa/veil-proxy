# B-352: 試験のポート選びが TCP しか見ず、Windows で UDP の予約範囲に当たって落ちる

**状態: 修正済み（feat/v0120-perf、2026-11-09）**

v0.12.0 の Windows のライブラリ E2E が 3 件落ちていた（L4 UDP echo の `WinError 10054`・HTTP/3 の接続タイムアウト・gRPC の interop）。
実体はこの 1 件。

## 事象

`free_port`（`tests/api/library_e2e.sh`・`tests/standalone/mod.rs`）は TCP の `bind(0)` で選んだ番号を UDP にも使う。Windows では Hyper-V・WSL・Docker が
UDP だけ予約する範囲があり（aofish では 50000–50059・50690–50789・50790–50889。`netsh int ipv4 show excludedportrange protocol=udp`。TCP の予約範囲とは別）、
TCP で空いていても UDP の `bind` が `WSAEACCES`（10013）で失敗する。Rust の showcase の L4 UDP が 50712、gRPC の HTTP/3 が 50864 に当たった。

## 原因

試験の仕組みの問題（製品のコードではない）。L4 の UDP と HTTP/3 は TCP と同じ番号の UDP を使う。

## 対応

両方の `free_port` を、TCP で取った番号の UDP も `bind` できることを確かめてから返すようにする。失敗した番号は次の周回で同じ番号を引かないように閉じずに持つ。

## テスト

Windows のライブラリ E2E・自己完結型 E2E が全部通ること。

結果: 修正の後は aofish で全部通った（ライブラリ E2E は 32 分、自己完結型 E2E は 48 件。`60fc2a63`。[platform_verification.md](../../perf/platform_verification.md)）。修正の前は UDP だけの予約範囲（50000–50059・50690–50889）
に当たったポートで `bind` が失敗してライブラリ E2E が落ちていた。
