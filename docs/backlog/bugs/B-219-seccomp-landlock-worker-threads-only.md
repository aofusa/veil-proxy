# B-219: seccomp と Landlock がワーカースレッドにしか掛かっておらず、制御プレーンのスレッドがサンドボックスの外で動いている

**状態: 未修正（2026-10-27 起票。セキュリティレビュー SB-01）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2、コミット 9449ac6a のコード読解。メモは `docs/artifacts/security/`、git 管理外）。

## 事象

- `security::apply_seccomp` は `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, ..)` でフィルタを付ける。`prctl` は**呼び出したスレッドにしか**
  掛からない（`seccomp(2)` の `SECCOMP_FILTER_FLAG_TSYNC` を使っていない）。`apply_landlock` の `landlock_restrict_self` も呼び出した
  スレッドの資格情報にしか付かない。
- ところが `entry.rs` では `apply_security_restrictions`（L915 付近）より**前**に、ログ（ftlog、L273）・OTel のエクスポータ（L511）・
  TLS 証明書のリローダ（L552）・VDS（L624）・ACME（L629）・設定リロード・ヘルスチェッカ・キャッシュの掃除・Wasm の tick・
  Wasm の gRPC（L713〜L727）の各スレッドを生成している。これらは起動後も**無制限のまま**動く。制限されるのは L1165 以降で
  生成するワーカースレッドだけ。
- VDS/xDS は制御サーバの応答（JSON・protobuf）を、ACME は CA の応答を解析するスレッドで、外部入力を扱うのにサンドボックスの外にいる。
- `docs/guide/security.md` の多層防御の図は「プロセス全体」を示唆していて実態と違う。`report_security_status` の表示も確認が要る。

## 影響

seccomp・Landlock は多層防御の 1 層で、これ自体で侵害が成立するわけではない。ただし制御プレーンのスレッドで任意コード実行が
起きたときに期待した封じ込めが無い。コンテナ（`docker/assets/security/seccomp.json`）では Docker の seccomp が全スレッドに掛かる。

## 改修案

- seccomp: `prctl` を `syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC, &prog)`（Linux 3.17+）に替える。
  TSYNC は既存の全スレッドに同じフィルタを同期して付ける（付けられないスレッドがあると失敗するので、戻り値を見て起動を止める）。
  `seccomp` の syscall 自体は適用後に使わないので許可リストに足さない。aarch64 の番号も確かめる（B-103 の手順）。
- Landlock: TSYNC 相当が無い。(a) スレッドを生成する前に `landlock_restrict_self` を済ませる（ログ・OTel 等の初期化順を入れ替える）か、
  (b) `std::thread::Builder` を包んでスレッドの開始時に同じ ruleset fd で `landlock_restrict_self` する。ruleset fd を保持しておけば各
  スレッドで適用できる。
- `docs/guide/security.md`（英・日）の多層防御の図と説明を実態に合わせる（修正後は「全スレッド」と書ける）。

## 受け入れ条件

- E2E（`tests/e2e_tests/security_audit.rs`）で、起動した veil の `/proc/<pid>/task/*/status` の `Seccomp:` が**全スレッド**で 2 であること。
  Landlock は `/proc/<pid>/task/*/status` に出ないので、制御プレーンのスレッドから許可外のパスを開いて EACCES になることを確かめる
  （例: TLS リローダの経路で許可外のファイルを読ませる）。
- 既存の E2E・`tests/e2e_setup.sh` の seccomp 有効の試験が通ること（F-279 で追加する）。

## 関連

- [B-220](B-220-seccomp-missing-dup-unlink-mkdir.md)（許可リストの不足）、[F-279](../features/F-279-security-test-gaps.md)（seccomp 有効の E2E）
- [B-103](B-103-seccomp-wrong-syscall-numbers.md)（番号の確かめ方）
