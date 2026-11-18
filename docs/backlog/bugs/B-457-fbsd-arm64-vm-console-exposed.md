# B-457: FreeBSD arm64 の VM の補助スクリプトが、認証なしのシリアルコンソールと SSH をホストの全インタフェースへ公開していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「FreeBSD arm64 VM helper publishes the guest's unauthenticated serial console (and SSH) on all host interfaces」から確認。

## 事象

- `tools/qemu/fbsd-arm64-vm.sh` の `up` は `docker run -p PORT:PORT`（ホストの IP なし）でコンテナを起動していた。
  Docker は全インタフェースで受け、iptables の規則でホストのファイアウォールも迂回する。コンテナの中の qemu は
  ゲストのシリアルコンソールを認証なしの telnet で出しており、ローダで `boot -s` とすればパスワード無しで root の
  シェルになる（`freebsd-provision.py` はこれで鍵を入れる）。同じネットワークの他のホストが VM を乗っ取れた。
- `bsd-vm.sh`・`aarch64-vm.sh` は以前に `-p 127.0.0.1:` へ直していたが、このスクリプトが残っていた。

## 対応

- SSH・コンソールの公開をループバック（`-p 127.0.0.1:…`）だけにした（兄弟のスクリプトと同じ）。

## テスト

- 起動の経路だけの変更（`bash -n`）。ログの案内（`telnet 127.0.0.1:…`・`ssh 127.0.0.1:…`）は元からループバックを指す。
