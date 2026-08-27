# B-76: `reuseport_balancing = "cbpf"` が全接続をワーカー 0 に固定する

## 事象

`[performance] reuseport_balancing = "cbpf"` を指定すると、`SO_REUSEPORT` の
全リスナーのうち **ワーカー 0 だけ**が接続を受理する。ワーカー数に関わらず
実質 1 コアで動作するため、スループットが kernel 既定の約半分になる。

`tools/perf` の 2026-08-26 フルスイート（`veil_glibc`、中央値 rps）:

| 構成 | HTTP/1.1 | HTTP/2 |
|---|---|---|
| `h2_1_ktls_0_lb_kernel_ofc_1` | 9,433 | 7,349 |
| `h2_1_ktls_0_lb_cbpf_ofc_1` | **5,865（0.62×）** | **5,712（0.78×）** |
| `h2_0_ktls_0_lb_kernel_ofc_0` | 8,393 | — |
| `h2_0_ktls_0_lb_cbpf_ofc_0` | **4,763（0.57×）** | — |

nginx ベースライン（同条件 TLS 静的、6,758 rps）に対しても **cbpf 構成だけが負けている**
（0.70×）。kernel 構成は 1.39× で勝っている。

## 原因

`src/system.rs::create_reuseport_cbpf_program` の cBPF プログラムが

```
LD  A, [0]          ; 「sk_reuseport_md のオフセット 0 = remote_ip4」のつもり
ALU A %= num_workers
RET A
```

となっているが、**この前提が誤り**である。`SO_ATTACH_REUSEPORT_CBPF` が受け取るのは
**classic BPF** であり、`sk_reuseport_md`（オフセットでフィールドを読む構造体）は
eBPF の `BPF_PROG_TYPE_SK_REUSEPORT` 専用のメタデータである。

classic BPF 版のリスナー選択（カーネル `net/core/sock_reuseport.c` の `run_bpf_filter`）は
実行前に `pskb_pull(skb, hdr_len)` で **データポインタをトランスポートヘッダの先へ進める**。
すなわち `[0]` が指すのは **TCP ペイロード**であり、リスナー選択が走る SYN パケットには
ペイロードが無い。classic BPF は範囲外ロードでプログラムを打ち切り **0 を返す**ため、
返り値（= リスナー配列のインデックス）は**常に 0**になる。

### 実証

4 本の `SO_REUSEPORT` リスナーへ 200 接続を流し、受理したリスナーを数える最小再現
（`accept` の分布）:

| cBPF プログラム | w0 | w1 | w2 | w3 |
|---|---|---|---|---|
| 現行（`LD [0]`） | **200** | 0 | 0 | 0 |
| `SKF_AD_CPU` | 0 | 200 | 0 | 0 |
| **`SKF_AD_RXHASH`** | **58** | **44** | **46** | **52** |
| `SKF_AD_RXHASH`（0 のとき `SKF_AD_CPU`） | 40 | 56 | 39 | 65 |

`SKF_AD_CPU` が偏っているのは再現テストのクライアントが単一スレッド（単一 CPU）で
接続しているため。**`SKF_AD_RXHASH`（`skb->hash` = フロー 4 タプルハッシュ）だけが
loopback でも正しく分散する。**

## 影響

- Linux で `reuseport_balancing = "cbpf"` を設定した全構成。
- **`examples/config.toml` / `contrib/config/config.toml` / README は `"cbpf"` を
  「推奨」として例示している**ため、ドキュメントどおりに設定したユーザは
  マルチワーカー並列性を完全に失う（既定値は `Kernel` なので既定構成は無傷）。

## 改修

`create_reuseport_cbpf_program` を **`SKF_AD_RXHASH`（skb->hash）ベース + `0` のときは
`SKF_AD_CPU` へフォールバック**する 5 命令のプログラムへ置き換える。

```
LD  A, [SKF_AD_OFF + SKF_AD_RXHASH]   ; A = skb->hash（フローハッシュ）
JEQ A, 0 -> +0 / +1                   ; hash が 0（未計算）なら CPU へフォールバック
LD  A, [SKF_AD_OFF + SKF_AD_CPU]      ; A = 受信 CPU
ALU A %= num_workers
RET A
```

同一フロー（= 同一 TCP 接続）は常に同じワーカーへ落ちるため、cBPF の目的である
「CPU キャッシュ効率・セッション再開効率」は維持される。ただし振り分けの単位は
**クライアント IP ではなく 4 タプルフロー**になるため、README / `examples/config.toml` /
`contrib/config/config.toml` / `src/config.rs` の doc コメントの説明を実装に合わせる。

## 再発防止

`create_reuseport_cbpf_program` の単体テストを「命令列の形の照合」だけでなく
**実際に `SO_REUSEPORT` リスナー 4 本へアタッチして accept 分布が 1 本に偏らないこと**
を検証する統合テストにする（Linux 限定）。
