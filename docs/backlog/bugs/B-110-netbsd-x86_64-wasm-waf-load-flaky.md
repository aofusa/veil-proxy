# B-110: NetBSD x86_64 の E2E で WAF の WASM テストが負荷時だけ失敗する

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

## 事象

NetBSD 10.1 x86_64（QEMU/KVM）のフル E2E で `test_http3_wasm_local_response` が 1 件失敗する
（2026-10-10、F-176 の検証で 564 / 1）。WAF モジュールが初回要求で正規表現を構築する処理が Pulley 実行で
実行予算を超えて trap し、fail-open で 200 を返すため、403 を期待するテストが落ちる。

- **単独実行では 3 回とも成功**（1.24〜1.38 秒）。2026-08 の同環境では 10〜12 件失敗していた。
- NetBSD は全アーキで常に Pulley（B-55）。OpenBSD の B-58（Pulley で CPU 律速の WASM が極端に遅い）と同種の
  負荷依存の現象と見ている。

## 追記（2026-10-27、Linux x86_64）

feat/v0100 の io_uring の E2E（この端末の 4 コア、同時にビルドなし）で 1 回だけ同じ失敗（`status=200`、上流の
`x-server-id: backend2`。WAF が fail-open）。続けて回した epoll・seccomp strict（io_uring）の E2E と、GitHub の
nightly の io_uring・epoll の E2E では通った。Pulley でない Linux でも、並列の試験の負荷の下で初回要求の正規表現の
構築が予算を超え得る（同じ現象）。

## 追記（2026-11-14、Linux x86_64）

feat/beat-nginx の最終確認の io_uring の E2E で 1 回だけ `status=503`・本文無し（既定が `fail_closed` になった F-293 の
後の同じ現象。503 はモジュールの失敗の応答）。同じ回の epoll の E2E では通った。

## 原因

WAF モジュール（`examples/wasm-filters/waf-filter`）は CRS の正規表現を、ルートコンテキストを作るとき
（`proxy_on_context_create`。インスタンスの起動時のコールバック）に組み立てる。veil は新しいインスタンスの起動時の
コールバックにも、1 回のコールバックと同じ実時間の締切（エポック 10 ティック、約 1〜1.1 秒）を掛けていた。起動時の
コールバックも燃料の `yield_interval_us`（既定 1ms）ごとに同じワーカーの他の要求へ譲るので、並列の試験の負荷の下では
CPU の量（燃料）は足りていても実時間が 1 秒を超え、`interrupt` でトラップする。Pulley は 1 命令が遅いので頻繁に起きた。

作りかけのインスタンスは捨てられ、次の要求がまた最初から組み立て直す。本番でも、負荷が続く間は起動の重いモジュールの
付いたルートがインスタンスを作れず、`fail_closed` の 503（`fail_open` ならフィルタを飛ばした応答）を返し続け得た。

起動時の検査（`validate`）は B-336 で実時間の締切を約 60 秒にしてあったが、要求の経路でインスタンスを作るときは
1 回のコールバックの締切のままだった。

## 修正

- 新しいインスタンスの起動時のコールバック（`_start` → root コンテキスト → `on_vm_start` → `on_configure`）の実時間の締切を、
  起動時の検査と同じ約 60 秒（`LIFECYCLE_EPOCH_DEADLINE`）にした（`FilterEngine::new_configured_instance`）。CPU は同じ燃料
  （`max_execution_time_ms`）で縛るので、暴走するモジュールは従来どおり止まる。インスタンスができたら、要求のコールバックには
  1 回のコールバックの締切（約 1 秒）を戻す（従来どおり）。
- `docs/guide/wasm.md`・`docs/guide/ja/wasm.md` の「失敗の扱い」に、起動時のコールバックの実時間の締切を書いた。

## 確認

- 単体試験 `b110_new_instance_lifecycle_survives_yielding_past_the_callback_deadline`: 燃料 1,000 ごとに譲らせ、最初に譲った
  ところでエポックを 1 回のコールバックの締切の 2 倍だけ進めても、`new_configured_instance` がトラップしない。できたインスタンスの
  要求のコールバックは、締切を超えてエポックを進めると `interrupt` でトラップする。修正前のコードでは起動時のコールバックの
  `proxy_on_context_create` が `wasm trap: interrupt` で落ちる。
- clippy（`full`・`full,epoll`、`-D warnings`）、WASM の単体試験 173 件。
- E2E: io_uring を 2 回・epoll を 1 回回して、594 件がすべて通った。
