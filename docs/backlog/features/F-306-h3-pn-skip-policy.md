# F-306: HTTP/3 でパケット番号を飛ばすのを輻輳窓が伸びたときだけにする

**状態: 実装中（feat/beat-nginx、2026-11-12）**

## 背景

F-305（GSO を既定で有効）・B-361 の後も、コンテナ間の HTTP/3（h2load の `--alpn-list=h3 -t4 -c100 -m10 -D 20`、
54,576 バイトの静的配信）で veil は nginx より 1 割ほど遅かった。

- サーバの CPU は veil の方が少ない（1 要求あたり 150µs 前後、nginx は 190µs 前後）。律速は負荷側の h2load
  （4 スレッドがほぼ飽和）で、rps は h2load が 1 要求に使う CPU で決まる。
- 輻輳制御（cubic・reno・HyStart の有無）・初期の窓・送信容量の係数・GSO の束の大きさを変えても差は縮まなかった（lab17〜lab19）。
- quiche は楽観的 ACK（受け取っていないパケットの ACK。RFC 9000 §21.4）を見つけるため、ハンドシェイクの後は、飛ばした番号が
  確かめられる（相手がそれより大きい番号を ACK する）たびに次の番号を飛ばす（輻輳窓の 0.5〜2 倍のパケットごと、およそ 1 往復に 1 回）。
  飛ばした番号は相手の受信の記録に接続の終わりまで穴として残る。ngtcp2（h2load・curl）はその記録（`ngtcp2_gaptr`、
  `ngtcp2_ksl` のノードを線形に探す）を受け取るパケットごとに引き、穴は 256 個まで残る。h2load の perf では
  `ksl_bsearch`・`ngtcp2_ksl_range_*_compar` が上位に出ていた。
- 一時的なビルドで飛ばす間隔を変えると rps は穴の数に応じて変わった（lab20）: 既定 35.4k、間隔 8 倍 36.9k、64 倍 37.7k、
  飛ばさない 39.5k（nginx 40.5k）。

## 対応

vendoring の quiche の `PktNumManager` に飛ばす頻度の方針（`PnSkipPolicy`）を足し、veil の `[http3] skip_packet_numbers` で選ぶ。

| 値 | 挙動 |
|---|---|
| `"on_growth"`（既定） | 輻輳窓が最後に確かめた窓の 2 倍に達したときだけ次を飛ばす |
| `"every_round"` | 確かめるたびに次を飛ばす（quiche の既定） |
| `"off"` | 飛ばさない |

- 「確かめた窓」は、飛ばす番号を仕掛けてから確かめるまでの間の最小の窓（送るたびに `min`）と、確かめた ACK の損失を反映した後の窓の
  小さい方（`on_skip_pn_validated(cwnd)`）。仕掛けた時点の窓にすると、確かめる間だけ正直に振る舞う相手が損失で窓が縮んだ後も高い値を
  確かめた窓として残し、次の閾値を吊り上げられる。
- 窓が伸びる間（スロースタート。楽観的 ACK で最も得をする局面）は、確かめた直後に窓がすでに 2 倍に達しているので、およそ 1 往復に
  1 回確かめる（`every_round` と同じ）。窓が伸びなくなった接続には穴を足さない。
- 損失を隠して窓を広げる相手は、次に仕掛けられるまで（窓が確かめた窓の 2 倍になるまで）は見つからない。仕掛けられた後に損失を隠し続ければ
  見つかって PROTOCOL_VIOLATION で閉じる。確かめる間だけ正直に振る舞えば、その間の損失で窓が縮み、それが次の確かめた窓になる。
  つまり送出を経路が実際に運べた窓の 2 倍程度より上に保ち続けることはできない（`every_round` は損失を隠し続ければほぼ必ず見つかる）。
  RFC 9000 §21.4 では番号を飛ばすこと自体が MAY で、比較対象の nginx は飛ばさない。
- ホットパスへの追加は、送るパケットごとの分岐 1 つと `min` だけ（飛ばす番号が仕掛けられている間）。

vendoring の quiche の差分は `third_party/quiche/README.veil.md` の 8（版 `0.24.9-veil.5`）。

## テスト

- quiche の単体: `skip_policy_on_cwnd_growth`（窓が 2 倍になるまで仕掛けない）・`skip_policy_on_cwnd_growth_measures_from_the_armed_window`
  （確かめる前に窓が伸びても仕掛けた窓で数える）・`skip_policy_on_cwnd_growth_validates_the_smallest_window`（確かめるまでの最小の窓・
  確かめた ACK の後の窓）・`skip_policy_on_cwnd_growth_waits_for_validation_and_handshake`・`skip_policy_off_never_skips`・
  `skip_policy_default_is_every_round`（`cargo test -p veil-proxy-quiche --features aws-lc-sys --lib packet::`）。
  quiche の接続の試験は証明書を置いて回し、失敗の一覧が変更の前と同じ（証明書に依存する既存の失敗だけ）であることを確かめた。
- veil の単体: `test_http3_config_default`（既定が `on_growth`）・`test_http3_skip_packet_numbers_parse`。
- E2E: HTTP/3 の E2E は既定（`on_growth`）で回る。

## 計測

aofish（Docker Desktop・WSL2、16 論理 CPU）のコンテナ間、h2load の HTTP/3（`-t4 -c100 -m10 -D 20`）、54,576 バイトの静的配信。
同じ周回の中で交互に回した。lab18〜lab21 は飛ばさない設定を一時的な環境変数で切り替えたビルド。

| 回 | nginx | veil（飛ばす。quiche の既定） | veil（飛ばさない） |
|---|---|---|---|
| lab18 | 42.4k | 37.3k | 40.2k |
| lab19 | 40.7k | 36.0k | 40.5k |
| lab20 | 40.5k | 35.4k | 39.5k |
| lab21 | 41.6k | 35.4k | 40.1k |

- 飛ばさないと veil のサーバの CPU は変わらず（1 要求 148〜153µs）、負荷側の CPU が下がって rps が 1〜2 割上がる。
- コンテナ間の経路のデバイス（コンテナ側・ホスト側の veth）の受信・送信の取りこぼしは 0（lab21）。
