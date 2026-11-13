# B-364: NetBSD aarch64 の検証用 VM（HVF）でネットワークの設定がカーネルの中で止まり、SSH が上がらない

**状態: 修正済み（feat/beat-nginx、2026-11-13）**

feat/beat-nginx の全プラットフォームの確認（mac の `tools/qemu/bsd-vm.sh netbsd aarch64`、QEMU 11.0.3 + HVF、NetBSD 10.1）で見つけた。
veil 本体ではなく検証用のハーネスの不具合。

## 事象

- `bsd-vm.sh netbsd aarch64 up` の後、`wait` が SSH に到達しない（1 時間 40 分待っても終わらない）。シリアルコンソールにはログインの
  プロンプトが出ている。
- ホストの転送（127.0.0.1:2360）は TCP を受けるが、ゲストの sshd のバナーが返らない（`Connection timed out during banner exchange`）。
- コンソールから入ると sshd は動いているが、`dhcpcd: [privileged proxy]` が D（割り込めない待ち）のまま。`ifconfig vioif0`・
  `netstat -rn` もカーネルの中で止まり、Ctrl-C も効かない。ACPI の電源断（QMP の `system_powerdown`）で止めて起動し直しても同じだった
  （2 回続けて）。同じ QEMU・同じ日の FreeBSD・OpenBSD の aarch64 の VM は問題なかった。

vioif（`virtio32: network device`、`CTRL_VQ` を含む機能）の受信フィルタ等の設定は制御キュー（ctrl_vq）へ命令を積んで完了を待つ。
その完了を待ったまま止まり、同じロックを待つ `ifconfig`・`netstat`・dhcpcd が連鎖して止まっていると見ている（`ctrl_vq=off` で解消した）。

## 対応

`tools/qemu/bsd-vm.sh` の aarch64 の起動（ネイティブの HVF・TCG と、helper コンテナの TCG の 2 箇所）で、NetBSD のときだけ
`-device virtio-net-pci,...,ctrl_vq=off` にする（`_aarch64_nic_extra`）。制御キューが無いと vioif は受信フィルタを設定しないだけで、
slirp（`-netdev user`）の通信は変わらない。他の OS の起動の引数は変えない。

## テスト

- `ctrl_vq=off` で起動し直すと SSH が通り、止まっていた `wait` がそのまま抜けて、続く単体・E2E の手順へ進んだ（2026-11-13 23:10）。
