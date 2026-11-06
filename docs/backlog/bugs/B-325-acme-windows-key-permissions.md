# B-325: Windows で ACME の秘密鍵・アカウント鍵が親ディレクトリの権限を継承する

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F25）で判明。`write_atomic` の `secret`（unix は 0600）と
`storage_dir` の 0700 は unix だけで、Windows では TLS の秘密鍵・仮の鍵・ACME のアカウント鍵を親の DACL を継承して
作っていた（Users が読める場所に置けば読める）。ACME は `full-windows` に入っている。

## 修正

- Windows では秘密のファイル（書く前の一時ファイル）と `storage_dir` に、継承を切った DACL
  `D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)`（SYSTEM・Administrators・所有者だけ）を `SetNamedSecurityInfoW` で付ける。
  失敗したら書かない。windows-sys に `Win32_Security_Authorization` を足した。

## 試験

- Windows 実機（aofish）でのビルドと ACME の E2E で確かめる。
- 文書: `docs/guide/acme.md`（英日）。
