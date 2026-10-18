# B-150: 試験用の Linux aarch64 VM の SSH を全アドレスで公開し、パスワード認証を許していた

**状態: 完了（feat/v080-release、2026-10-18。F-236 の再走査の F9）**

## 症状

`tools/qemu/aarch64-vm.sh` の Docker のモードは `docker run -p "${SSH_PORT}:2222"` で SSH を 0.0.0.0 に公開し、cloud-init は
固定のパスワード（`veil:veil`・`root:root`）と `ssh_pwauth: true`、`veil` の NOPASSWD sudo を設定していた。同じネットワークの誰でも
VM の root を取れた（開発用の VM に限る）。

## 修正

公開を `-p 127.0.0.1:${SSH_PORT}:2222` にし、`ssh_pwauth: false` にする（鍵認証は既に設定している）。パスワードはシリアル
コンソール用に残す。

## 関連

F-236、B-151。
