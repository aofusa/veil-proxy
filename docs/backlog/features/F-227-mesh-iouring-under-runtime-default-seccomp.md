# F-227: seccomp の RuntimeDefault の下でのメッシュの io_uring 版（Linkerd）

**状態: 未着手（2026-10-16 起票・設計。親 F-215 の残件）**

## 背景

- containerd / CRI-O の `RuntimeDefault` の seccomp プロファイルは、`io_uring_setup`・`io_uring_enter`・`io_uring_register` を拒否する。
- Linkerd の注入は、プロキシのコンテナに `securityContext.seccompProfile: RuntimeDefault` を付ける。そのため io_uring のビルドの
  veil は起動時に io_uring を使えない。今は理由を表示して終了している（`src/mesh/server.rs`）。
- Istio のサイドカー（`istio-proxy`）には seccomp の指定が無いので、io_uring 版でも動く（smoke 26/26・tasks 18/18）。
  ただし、kubelet の `seccompDefault: true` のノードや、Pod 側で RuntimeDefault を付けたワークロードでは、Istio でも同じことが起きる。
- 今の既定のイメージは epoll 版（`--features mesh-istio,mesh-linkerd,epoll,wasm`）なので、どこでも動く。この問題があるのは
  io_uring 版を選んだ場合だけである。

## 段階 0: io_uring にする価値があるかを先に測る

- Istio（io_uring が動く）で、io_uring 版と epoll 版のイメージを `tools/k8s/mesh-bench.sh` で交互に測る。
  - 経路: サイドカー（HTTP/1.1・gRPC）とゲートウェイ。
  - 指標: 最大 rps・p99（1,000 rps 時）・要求あたりの CPU。
- 判断の規則: どの経路でも最大 rps と要求あたりの CPU の差が 3% 未満なら、ここで閉じる。
  - 閉じる場合は、epoll 版をメッシュの唯一のイメージと書き（`docs/guide/mesh.md`）、io_uring 版の起動時の終了は残す。
  - メッシュの接続は短い要求が多く、io_uring の一括投入が効きにくい可能性がある。それで先に測る。
- 3% 以上の差があれば、段階 1 に進む。

## 段階 1 の設計: 起動時に epoll 版へ切り替える

**採る案:** イメージに両方のバイナリを入れる。io_uring 版が起動時に io_uring を使えなければ、epoll 版を `execve(2)` する。

- 配置: `/usr/lib/linkerd/linkerd2-proxy`（io_uring 版）と `/usr/lib/linkerd/linkerd2-proxy.epoll`（epoll 版）。
  Istio では `/usr/local/bin/envoy` と `envoy.epoll` になる。
  - 切り替え先は環境変数 `VEIL_MESH_FALLBACK_BIN` で上書きできる。既定は「自分の実行ファイルのパス + `.epoll`」で、
    `/proc/self/exe` から決める。
- 置き場所: `src/mesh/server.rs` の今の io_uring の可否の確認の位置で行う。
  - スレッド・ソケット・制御プレーンへの接続を作る前に行う。
  - 引数と環境変数はそのまま渡す。argv[0] も変えないので、Envoy / Linkerd としての起動の判定も変わらない。
  - 失敗の理由（`EPERM`・`ENOSYS`）を warn で 1 回出す。
  - 切り替え先が無いときは、今と同じく理由を出して終了する。
- 再帰しない: epoll 版は io_uring を確認しない。
- Linkerd の起動の順は崩れない。`linkerd2-proxy-identity` が鍵と CSR を作ってからプロキシ本体を `exec` するので、そのあとで
  もう一度 `exec` しても鍵のファイルは同じである。
- イメージ: `docker/Dockerfile.musl` を 2 回ビルドして（features の違い）、`Dockerfile.mesh-istio`・`Dockerfile.mesh-linkerd` で両方を
  コピーする。イメージの大きさの増分を測って `docs/guide/mesh.md` に書く。

**採らない案:**

- **1 つのバイナリに両方のランタイムを入れ、起動時に選ぶ。** ランタイムのファサード（`runtime::tcp` など）は今は cfg で
  静的に選んでいる。動的に選ぶと、I/O の操作ごとに分岐か間接呼び出しが増える。また、AGENTS.md の「io_uring パスのロジックは
  reactor 追加で変えない」に反する。メッシュ以外の full のバイナリにも影響する。
- **io_uring を許す Localhost の seccomp プロファイルを配る**（RuntimeDefault + `io_uring_*`。security-profiles-operator 等）。
  - Linkerd の注入はコンテナ単位で RuntimeDefault を付ける。コンテナ単位の指定は Pod 単位より優先されるので、注入の後で書き換える
    webhook が要る。
  - io_uring を許すと seccomp による防御が弱まる。RuntimeDefault が io_uring を拒否するのは、カーネルの攻撃面を減らすためである。
  - そのため既定にはせず、運用者向けの選択肢として `docs/guide/mesh.md` に書くだけにする。

## 受け入れ条件

- 段階 0 の計測の結果と判断を、`docs/perf/README.md` の「サービスメッシュ」に書く。
- （段階 1 に進んだ場合）io_uring 版のイメージを使った次の試験:
  - Linkerd の all が epoll 版への切り替えで通る。
  - Istio で、Pod に `seccompProfile: RuntimeDefault` を付けたワークロードの smoke が通る。
  - seccomp の無い Pod では io_uring のまま動く（ログで確かめる）。
- 単体: 切り替え先のパスの決め方、`VEIL_MESH_FALLBACK_BIN`、切り替え先が無いときの終了。
- （必須、F-215）既存のプロキシの性能が退行しない（`suite_ab.sh`）。メッシュのコードは full のバイナリに入らない。イメージは musl 版。

## 関連

F-215、F-216、F-223、`src/mesh/server.rs`、`docker/Dockerfile.mesh-*`。
