# B-163: 平文のリスナーだけの設定では、FreeBSD は起動を中止し、OpenBSD の pledge・macOS の sandbox_init は掛からない

**状態: 完了（feat/distribution-tickets、2026-10-23。F-247 のアーカイブの起動の試験で発見）**

## 現象

`[server].listen` が無く `listen_cleartext` だけの設定（F-247 のパッケージ・アーカイブの既定の設定）で:

- **FreeBSD:** `capsicum_capability_mode = true` だと `capability mode requested but [server].listen_cleartext requires
  an extra listener; aborting startup` で起動しない（終了コード 1）。
- **OpenBSD:** `enable_pledge = true` でも pledge が掛からない。ログは何も言わない。
- **macOS:** `enable_sandbox_macos = true` でも sandbox_init が掛からない。ログは何も言わない。

## 原因

- bind の完了を待ってからサンドボックスに入るバリア（capability mode・pledge は bind を禁じるため）は、TLS のリスナーの
  ワーカーだけを数えていた。
- 待つスレッド自体も、TLS のリスナーがあるときにしか起動していなかった。
- FreeBSD は、数えられない平文のリスナーを「capability mode に入れない理由」として起動を中止していた（F-181 の fail-closed）。
  OpenBSD・macOS は待つスレッドが無いので、黙ってサンドボックス無しで動いていた。
- OpenBSD・macOS は、時間切れでも「NOT applied」と記録して続けていた（`allow_security_failures = false` でも）。

## 修正

- バリアが待つ数を「TLS のリスナーのワーカー数 + 平文のリスナーのワーカー数」にし、平文のワーカーも bind の後に数える。
- 待つスレッドを、TLS のリスナーの有無に依らず起動する。
- FreeBSD: 平文のリスナーを capability mode の阻害要因から外した（その bind もバリアで待つ）。
- OpenBSD・macOS: 時間切れは `allow_security_failures = false`（既定）なら起動を中止する（FreeBSD・F-181 と同じ）。
- 確かめたこと: CI の FreeBSD・OpenBSD・macOS の runner で、パッケージ・アーカイブの既定の設定で起動して `/`・h2c が 200、
  起動のログにサンドボックスを掛けた行が出る（`tools/release/binary-smoke.sh`）。

## 関連

- [F-247](../features/F-247-distribution-foundation.md)
- F-181（capability mode の fail-closed）・F-200（`listen_cleartext`）
