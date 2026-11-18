# B-462: Windows で ACME の秘密鍵の一時ファイルを共有を許して作り、DACL を絞る前に他の利用者がハンドルを開けた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Windows: the ACME TLS private key's temp file is created with the parent's inherited DACL and only locked down
afterwards, so a handle opened in that window can still read the key」から確認。

## 事象

- `acme::write_atomic` は一時ファイルを `create_temp` で作り（B-398 の O_EXCL）、Windows では作った後で
  `restrict_to_owner` で所有者だけの DACL にしていた（B-325）。作った直後の一時ファイルは親ディレクトリから継承した
  DACL（`%ProgramData%` の下等では Users が読める）を持ち、std の既定の共有モード（読み・書き・削除）で開いていたので、
  DACL を絞るまでの間に他の利用者がハンドルを開けば、アクセスはハンドルを開いたときに検査されるため、その後に書いた
  アカウントの鍵・証明書の秘密鍵をそのハンドルから読めた。

## 対応

- Windows では一時ファイルを共有モード 0 で作る。作成と同時に開くので隙間は無く、このハンドルを閉じるまで誰もデータを
  読み書きするハンドルを開けない。DACL は閉じる前に絞り（DACL の変更は共有モードの検査を受けない）、rename の前に
  ハンドルを閉じる。

## テスト

- Windows の単体試験 `acme::tests::temp_files_are_not_shared_on_windows`（作った一時ファイルを別に開くと
  `ERROR_SHARING_VIOLATION`）と既存の `secret_files_are_owner_only` を aofish の実機で確かめた。
