# B-94: HTTP/3 でクライアントの Initial が再送されるたびに別の新規接続を作っていた

## 事象

FreeBSD aarch64 の perf（h3load `-c64 -m32`）で、64 接続中 6〜20 本が毎回
「QUIC ハンドシェイクがタイムアウトした」（10 秒）になっていた。nginx は 0〜1 本。
タイムアウトした接続の 32 ストリームは計測中ずっと遊ぶので、h3_file の対 nginx 比が
0.65〜0.86 に落ちる主因の 1 つだった。

## 原因

`process_datagram_segments` は未知の DCID を持つ Initial を受けると、**乱数で SCID を作って**
`quiche::accept` し、その SCID だけをキーに接続を登録していた。クライアントはサーバの最初の
応答を受け取るまで Initial を**元の DCID 宛て**に送り続ける（PTO による再送、複数パケットに
またがる ClientHello）ため、それらは登録キーに一致せず、届くたびに**別の新規接続**として
`accept` されていた。孤児接続が増えるうえ、クライアントには SCID の異なる 2 つのサーバ接続から
応答が届き、ハンドシェイクが崩れる。負荷でサーバの初回応答が PTO（初期 RTT 333ms → 約 1 秒）
より遅れると必ず踏む。

## 修正

quiche のサンプルサーバと同じく、サーバ接続 ID を**クライアントの元 DCID から決定的に導出**する
（`SCID = HMAC-SHA256(プロセス鍵, client DCID)[..20]`、鍵は起動時に乱数で 1 回生成）。
未知の DCID の Initial を受けたら導出 SCID で既存接続を引き、あればそこへ配送する。
HMAC を計算するのは「未知の DCID を持つ Initial」だけで、確立済み接続のデータグラムでは
計算しない。HMAC はプラットフォームの暗号プロバイダ（`tls_provider::hmac` = aws-lc-rs / ring）。

## テスト

- `http3_server::tests::test_b94_derive_server_cid_is_deterministic_and_keyed`
- `http3_server::tests::test_b94_retransmitted_initial_maps_to_same_connection`
  （quiche クライアントの Initial を 2 回投入し、接続が 1 つで、2 回目が**既存の接続へ**届く
  ことを確認。導出 SCID による検索を外すと失敗する）

## 結果

FreeBSD aarch64 でハンドシェイクのタイムアウトが 6〜20 本 → 0〜4 本（B-95 と合わせて 0）。
