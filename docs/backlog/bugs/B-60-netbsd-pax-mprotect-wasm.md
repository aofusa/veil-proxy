# B-60: NetBSD の PaX MPROTECT により WASM フィルタの実行が `Permission denied` で失敗する

**状態: 対応済み（2026-08-06、実機 NetBSD 10.1 x86_64・evbarm-aarch64 の両方で検証）**

## 事象

NetBSD 10.1（実機 QEMU、x86_64・evbarm-aarch64 とも、`--no-default-features
--features full-netbsd`/`full-netbsd`）で、WASM モジュールの**ロードは
成功する**が、フィルタを**実行すると**必ず失敗する。

```
ERROR [src/wasm/engine.rs:128] [wasm:header_filter] on_request_headers error: Permission denied (os error 13)
```

E2E への影響は大きい: WASM を使う全テストがまとめて失敗する一方、WASM を使わない
テストは全て通るため、集計だけ見ると「一部のテストだけ失敗している」ように見える
（実測: 507 passed / 35 failed、失敗の 35 件は全て WASM 系テスト）。

## 原因

NetBSD は **PaX MPROTECT をデフォルトでシステム全体に強制**している:

```
$ sysctl security.pax.mprotect.enabled security.pax.mprotect.global
security.pax.mprotect.enabled = 1
security.pax.mprotect.global = 1
```

PaX MPROTECT は「一度書き込み可能にしたページを後から実行可能にする」
（またはその逆の）`mmap`/`mprotect` 呼び出しを拒否するカーネルの保護機構。
wasmtime はランタイムが JIT コード領域（Pulley インタープリタ実行でも wasmtime
自体のコード生成・実行用メモリ領域）を確保する際にこのパターンの `mmap`/
`mprotect` を行うため、PaX MPROTECT が有効なプロセスでは `EACCES` で失敗する。

これは **OpenBSD の `wxallowed`/`MAP_STACK` 強制（B-52）の NetBSD 版**と言える
現象で、根本原因（wasmtime が実行可能メモリの動的な確保・属性変更を要求する
こと）は共通だが、カーネル側の防御機構は別物（OpenBSD = SP が `MAP_STACK`
領域を指すことの強制、NetBSD = W^X 遷移そのものの禁止）であり、修正方法も別。

OpenBSD の Pulley 化（B-52 対応 3）はネイティブコード生成を避けることで
「配布時に `wxallowed` マウントが不要」という利点があったが、**NetBSD の
PaX MPROTECT は Pulley インタープリタ実行時にも発火する**（wasmtime 自身の
実行に使うメモリ確保が対象になるため、生成コードがネイティブか Pulley 用の
バイトコードかは無関係）。したがって B-52 と異なり、wasmtime 側の設定変更
（アロケータ戦略・ターゲット指定）では回避できない。

## 調査

- `on_request_headers`/`on_response_headers` など WASM フィルタを実際に**呼び出す**
  タイミングでのみ失敗し、モジュールのロード（コンパイル・インスタンス化前段）は
  成功することから、失敗箇所は「wasm バイナリの解析」ではなく「実行用メモリの
  確保」であると切り分けた。
- `sysctl security.pax.mprotect.*` を確認したところ、NetBSD のデフォルトインストール
  で `enabled=1`/`global=1`（システム全体で有効、個別バイナリの ELF フラグに
  かかわらず強制）であることを確認した。
- `paxctl <veil binary>` で現在のフラグを確認し、`paxctl +m <veil binary>`
  （小文字 `m` = MPROTECT 制限を**明示的に無効化**するフラグを立てる。大文字
  `M` は逆に**明示的に有効化**するフラグなので混同しないこと）を適用した後は
  `[wasm:header_filter] Added request headers for context 2` がログに出て、
  E2E の `X-Veil-Processed` ヘッダ検証も通ることを確認した。

## 対応

コード側（`src/`）での回避策は無い（PaX MPROTECT はプロセスの ELF ノート属性で
制御するものであり、wasmtime 側の実行方式を変えても迂回できない）ため、
**バイナリに `paxctl +m` を適用する運用対応**を、ビルド・検証・配布の各段階に
組み込んだ:

1. **`tests/e2e_setup.sh`**（`ensure_veil_binary`）: ホストが NetBSD かつ
   `/usr/sbin/paxctl` が存在する場合、ビルド済み veil バイナリへ自動的に
   `paxctl +m` を適用する（失敗しても警告のみで E2E は継続、他 OS では no-op）。
2. **`tools/qemu/bsd-vm.sh`**（`cmd_build`）: NetBSD ゲストのリリースビルド
   完了直後に、VM 内で同じ `paxctl +m` をゲスト側バイナリへ適用する
   （`paxctl` が無いイメージでは警告のみ、他 OS ゲストでは no-op）。
3. **`packaging/scripts/build-bsd.sh`**: `--os netbsd` でパッケージ化する際、
   実行ホストに `paxctl` があればステージング済みバイナリへ適用する。
   （パッケージ作成は通常 Linux ホストから行うため大半のケースでは適用でき
   ない。その場合は生成される `INSTALL.txt` に `paxctl +m /usr/local/bin/veil`
   を必須の追加インストール手順として明記する。）
4. **ドキュメント**: `AGENTS.md`（プラットフォーム別セキュリティの記述）・
   `README.md`/`docs/readme/README.ja.md`（NetBSD の対応表）・
   `packaging/README.md`（NetBSD 対応の現状の節）に、`paxctl +m` が必須である
   ことと上記 3 箇所への組み込みを明記した。

## 検証結果（2026-08-06、実機 QEMU: local Linux + KVM for NetBSD x86_64、Apple Silicon macOS + HVF for NetBSD aarch64）

NetBSD x86_64 の E2E（`tests/e2e_setup.sh test`、`full-netbsd`）は `paxctl +m` 適用前後で:

| 状態 | 結果 |
|---|---|
| `paxctl +m` 適用前 | 507 passed / 35 failed（失敗 35 件は全て wasm テスト） |
| `paxctl +m` 適用後 | 530 passed / 12 failed（残り 12 件は concurrent/stress 系 4 件・HTTP/3 系 7 件・rate limiting 1 件で、いずれも Docker ビルドと並走した高負荷下での実行が原因。wasm 単体テストは全て pass） |

NetBSD evbarm-aarch64（`full-netbsd`）は `TEST_FILTER=wasm_tests` で
`test result: ok. 23 passed; 0 failed; 519 filtered out`（`paxctl +m` 適用後、wasm 関連は全て pass）。

## 残課題

- wasmtime が **Pulley インタープリタ実行時にも**実行可能メモリの動的な
  mmap/mprotect を要求する点そのものは変わっていない。上流（wasmtime）で
  「実行可能コードを一切生成しない・実行時に W^X 遷移を行わない」経路が
  提供されれば、NetBSD 側でも `paxctl` による運用回避が不要になる可能性が
  あるが、現状ではそのようなモードは無い。
- `paxctl +m` はバイナリの再配置・再ビルドのたびに再適用が必要（ELF に
  埋め込まれるフラグのため、バイナリを差し替えると失われる）。CI/CD で
  自動配布する場合は本チケットで組み込んだ 3 箇所のいずれかを必ず経由させる
  運用を徹底すること。
- パッケージ配布時に `paxctl` が無いホストでビルドした場合、インストール手順を
  読み飛ばすと wasm フィルタだけが無言で `EACCES` になる（本体はエラーログを
  出すので気付けるが、監視していないと見逃しうる）。将来的には起動時に
  `security.pax.mprotect.global`（NetBSD のみ）を検出し、wasm 機能が有効な
  構成であれば起動時ログで注意喚起することを検討してもよい（未実装）。

## 関連

- B-52（OpenBSD の `wxallowed`/`MAP_STACK` 強制による WASM SIGSEGV。カーネル側
  防御機構は別だが、「wasmtime が実行可能メモリの確保を必要とする」という
  根本原因は共通）
- B-55（crates.io wasmtime が NetBSD（全アーキ）・FreeBSD/OpenBSD aarch64 の
  シグナルベーストラップに非対応で、`third_party/wasmtime` vendoring +
  Pulley 強制で解消した件。本チケットは B-55 解消後、実際に NetBSD 実機で
  wasm を動かして初めて顕在化した）
- F-140（NetBSD 対応。本チケットは F-140 の実機検証から得られた知見）
