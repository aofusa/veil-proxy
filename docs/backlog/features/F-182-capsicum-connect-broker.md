# F-182: capsicum の capability mode 下でも上流へプロキシできるようにする（接続ブローカー）

**状態: 完了（feat/f182-capsicum-connect-broker）**

## 背景

capability mode（`cap_enter(2)`）では `connect(2)`・`bind(2)`・名前解決ができないため、プロキシ構成では
capability mode を使えない（F-181 で、要求したのに入れない場合は起動を中止するようにした）。FreeBSD で最も強い
サンドボックスが静的配信専用になっており、用途が狭すぎる。

## 設計

### 全体

`cap_enter` の前に、サンドボックスの外で動く **接続ブローカー** プロセスを起動する。ブローカーは
**起動時に確定した許可リスト** の上流にだけ非ブロッキング `connect(2)` を開始し、そのソケットを `SCM_RIGHTS` で
veil 本体へ渡す。本体は受け取った fd の接続完了（writable + `SO_ERROR`）を従来の Connect と同じ手順で待ち、
以後の TLS・HTTP/1.1・HTTP/2 は従来どおり本体が行う。

```
veil 本体（capability mode）                     接続ブローカー（capability mode の外）
  connect(target) ─ 許可リストの添字を引く
  socketpair(返信用 a, b)
  sendmsg(制御ソケット, [magic, 添字], SCM_RIGHTS b) ─▶ recvmsg → 添字の範囲・fd の種別を検査
                                                      socket + 非ブロッキング connect（EINPROGRESS）
  a の readable を待つ（reactor）            ◀────── sendmsg(b, [errno], SCM_RIGHTS 接続 fd)
  接続 fd の writable + SO_ERROR を待つ
```

### 許可リスト（セキュリティの要）

- 本体からブローカーへ渡すのは **許可リストの添字（u32）だけ**。アドレス・ホスト名・パスの文字列は受け付けない。
  本体が乗っ取られても、接続できるのは設定に書かれた上流だけ（任意宛先への踏み台にならない）。
- 許可リストは起動時に設定から作る: `[upstreams]` の全サーバ、`Proxy` ルートの URL、OpenTelemetry の
  エンドポイント。種類は IP リテラル（`SocketAddr`）・ホスト名（`host:port`）・UDS パスの 3 つ。
- 許可リストは起動後に **変えない**（ブローカーは追加要求を受け付けるプロトコルを持たない）。リロードで新しい
  上流を足す設定は F-178 の規則で拒否する（再起動で反映）。同じ上流のままの変更（重み・ヘルスチェック・ルート等）は通る。
- ホスト名はブローカーが解決する（本体は capability mode のため DNS を引けない）。ブローカー内の解決スレッドが
  起動時に解決し、30 秒ごと（`runtime::dns::CACHE_TTL` と同じ）に更新する。接続要求の処理では DNS を待たない。
  未解決の間は `EHOSTUNREACH` を返し、1 秒ごとに解決を再試行する。最初のアドレスを使う（`runtime::dns` と同じ）。

### プロトコル

- 制御ソケット: `SOCK_SEQPACKET` の socketpair（メッセージ境界が保たれ、1 回の `sendmsg` が原子的）。
  ブローカー側の端は子プロセスの標準入力として渡す（他の fd は継承しない）。
- 初期化: 本体は許可リストを 1 エントリ 1 メッセージで送り（種別 1 バイト + 文字列、上限 4096 バイト）、終端を送る。
  ブローカーは初回の名前解決を終えてから `READY` を返す。本体は最大 10 秒待つ。
- 要求: 8 バイト（magic u32 + 添字 u32、リトルエンディアン）+ 返信用ソケット 1 本（`SCM_RIGHTS`）。
- 返信: 4 バイト（errno i32、0 は成功）+ 成功時のみ接続中ソケット 1 本。
- ブローカーは長さ・magic・制御メッセージの数と種類（`MSG_CTRUNC` を含む）・返信用 fd が AF_UNIX ソケットであることを
  検査し、不正な要求は受け取った fd をすべて閉じて捨てる（応答しない）。
- 返信用 socketpair を要求ごとに作るので、同じ制御ソケットを全スレッドで共有しても応答の取り違えが起きず、
  reactor の fd ごとの待機者（`WakerSlot`）も衝突しない。

### 本体側の差し込み口（FreeBSD のみ。他 OS とブローカー未起動時は 1 回の `OnceLock` 読み出しで素通り）

- reactor の `TcpStream::connect` / `connect_unix` / `connect_str`（データプレーンの上流接続はすべてここを通る:
  `proxy::connect_target`・バックグラウンド再検証・`upstream_mux` 等）。
- 同期の接続: `upstream::tcp_connect_timeout`（WASM の http_call / gRPC）、`upstream::connect_probe`（ヘルスチェック）、
  OpenTelemetry のエクスポータ。
- 許可リストに無い宛先は `PermissionDenied` で即座に失敗させる。

### ブローカーの防御

- 起動: `std::process::Command` で自身を再実行（隠し引数）。fork だけでマルチスレッドの状態を引き継がない。
  特権降格の後に起動するので、本体と同じ（降格後の）利用者で動く。
- 起動直後: `closefrom(3)`、`procctl(PROC_TRACE_CTL_DISABLE)`（ptrace とコアダンプを禁止）、
  `procctl(PROC_PDEATHSIG_CTL, SIGKILL)`（本体が死んだら道連れ）、`RLIMIT_NPROC = 0`（fork・exec 不可）、
  `RLIMIT_FSIZE = 0`・`RLIMIT_CORE = 0`、`RLIMIT_NOFILE` を小さく。
- 入力は固定長の要求だけで、文字列の解析は起動時の許可リスト受信に限る。
- 本体が終了すると制御ソケットが EOF になり、ブローカーも終了する。ブローカーが死んだら本体はそれを検知し、
  上流へつなげないまま動き続けないよう終了コード 1 で終了する（fail-closed。サービスマネージャの再起動に任せる）。
- ブローカーを起動できない場合は F-181 と同じく既定で起動を中止する（`allow_security_failures = true` なら
  rights 制限のみで続行）。

### 性能

- 代行のコストは **新規の上流接続のときだけ**（socketpair 1 + sendmsg/recvmsg 各 2 + close 2 と、ブローカーの
  コンテキストスイッチ）。上流は接続プール（HTTP/1.1）・多重化（HTTP/2）で再利用するので、定常状態の要求には
  コストが乗らない。リクエストごとのアロケーション・ブロッキングは無い（返信待ちは reactor の readiness）。
- capability mode を使わない構成、FreeBSD 以外の OS では、コードパスは変わらない。

### 対象外（引き続き capability mode に入れない）

L4 リスナー・h2c リスナー・HTTP/3・HTTP リダイレクトリスナー（いずれも `cap_enter` 後の `bind(2)` が要る）。

## 実装

| 箇所 | 内容 |
|------|------|
| `src/connect_broker.rs` | プロトコル（SCM_RIGHTS の送受信と検査）・許可リスト・ブローカーのループと解決スレッド・クライアント（unix 共通）。起動（`start`）・監視スレッド・防御（`harden`）・同期接続の補助（`connect_std_*`）は FreeBSD 専用 |
| `src/runtime/reactor/tcp/unix.rs` | `BrokerConnect`（FreeBSD 専用）。`connect` / `connect_unix` / `connect_str` はブローカー起動時だけこれを使う |
| `src/upstream.rs`・`src/otel.rs` | ヘルスチェック・WASM の外部呼び出し・OpenTelemetry の同期接続をブローカー経由にする |
| `src/config.rs` | `startup_connect_allowlist`（許可リスト）、capability mode の阻害要因から Proxy / `[upstreams]` を外す、リロード時は許可リスト外の上流を拒否 |
| `src/entry.rs` | 隠し引数でブローカーとして起動、capability mode に入る構成で上流があればブローカーを起動（失敗時は fail-closed） |

## 検証（2026-10-10）

- Linux: 単体 1004（ブローカーのプロトコル 5 件: 許可された TCP・ホスト名・UDS への接続、範囲外の添字と接続拒否の
  errno、不正な要求（magic・長さ・fd なし・fd 2 本・ソケット以外の fd）を捨てて処理を続けること、許可リストの
  初期化メッセージの検査、重複の統合）・統合 54・E2E 565/565（io_uring・epoll）。clippy 警告なし
- FreeBSD 14.3 x86_64（QEMU/KVM）: 単体 966/0・統合 54/0・E2E 565/0（capability mode を使わない経路の回帰なし）
- FreeBSD security-e2e: PASS。capability mode のまま、IP・ホスト名・UDS・ヘルスチェック付きグループの上流へ
  プロキシして 200。ブローカーは子プロセスとして本体と同じ利用者で動く。同じ上流のままのリロードは通り、許可リスト外の
  上流を足すリロードは拒否。ブローカーを kill すると本体は終了コード 1。F-178・F-181 の確認も引き続き PASS

## 性能

- **定常状態**（上流はプールで再利用）: FreeBSD VM（4 vCPU、h2load も同じ VM）で HTTP/1.1・32 接続・3 バイトの
  プロキシ、交互 6 ラウンド。capsicum なし 中央値 6,931 rps、capability mode + ブローカー 中央値 6,807 rps（−1.8%）。
  ラウンド間のばらつき（±8%）の範囲で、差は無い。全要求成功。
- **新規の上流接続 1 本あたり**: Linux の単体テストの同期経路で、直接 connect 約 57µs に対しブローカー経由
  約 145〜195µs（+90〜140µs。プロセス間の起床 2 回と fd の受け渡し）。TLS ハンドシェイク（ミリ秒単位）に比べて小さく、
  プール・多重化で再利用する要求には乗らない。
