# B-102: 不正な HTTP/2 プリフェースに GOAWAY を返さず RST していた（h2c）

## 事象

`tools/container_security` の h2spec（`H2SPEC_FULL=1`）で、h2c だけ 146 件中 1 件失敗:

```
3.5. HTTP/2 Connection Preface
  × 2: Sends invalid connection preface
    Expected: GOAWAY Frame (Error Code: PROTOCOL_ERROR) / Connection closed
      Actual: read: connection reset by peer
```

TLS 側は通過していた。

## 原因

h2c 専用リスナーはプロトコル検出（MSG_PEEK）で HTTP/2 プリフェースでないものを
`Unknown` とし、何も送らずに接続を drop していた。ソケットに未読データが残ったまま
close すると TCP は FIN ではなく RST を送るため、相手からは GOAWAY も正常クローズも
見えない。`Http2Connection::handshake` の `InvalidPreface` も同様に何も送らずに
エラーを返していた。

## 修正

RFC 9113 §3.4（不正なプリフェースは PROTOCOL_ERROR のコネクションエラー）に従い、
両方の経路で GOAWAY(PROTOCOL_ERROR) を送り、送信側を閉じ（`shutdown(Write)`）、相手の
close まで時間・量の上限付き（500ms / 64KB）で読み捨ててから閉じる。エラー経路のみで、
ホットパスは無変更。

## テスト

- 単体: `http2::connection::tests::invalid_preface_sends_goaway_protocol_error`
- h2spec（h2c / TLS）: 0 failed
