/*
 * veil.h - veil API サーバ基盤ライブラリの C API (F-193)
 *
 * C のコールバックを HTTP ハンドラとして登録し、veil_run() で HTTP/1.1・HTTP/2 (h2c / TLS)・
 * HTTP/3 のサーバを起動する。libveil.so / libveil.a とリンクする。
 *
 * スレッドモデル:
 *   - 既定ではコールバックはハンドラ用プールのスレッドで実行される (複数スレッドから同時に呼ばれる)。
 *   - VEIL_ROUTE_INLINE を付けたルートはワーカースレッド上で実行される (ブロックしてはならない)。
 *   - user_data は複数スレッドから同時に使われても安全でなければならない。
 */
#ifndef VEIL_H
#define VEIL_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- エラーコード ---- */
#define VEIL_OK              0
#define VEIL_ERR_INVALID_ARG (-1) /* 引数が NULL・不正 */
#define VEIL_ERR_CONFIG      (-2) /* 設定の読み込み・上書きの失敗 */
#define VEIL_ERR_RUNTIME     (-3) /* 起動・実行時のエラー */
#define VEIL_ERR_UTF8        (-4) /* 文字列が UTF-8 ではない */
#define VEIL_ERR_CLOSED      (-5) /* 相手が切断した・ストリームが閉じている (F-210) */
#define VEIL_ERR_STATE       (-6) /* 呼べない状態 (INLINE でのストリーミング・単項の 2 回目の送信等) */
#define VEIL_ERR_FORM        (-7) /* フォームの本文が壊れている (400 を返す。F-266) */
#define VEIL_ERR_TOO_LARGE   (-8) /* フォームの本文が [api] multipart_* の上限を超えた (413 を返す。F-266) */

/* ---- ルートのフラグ ---- */
/* コールバックをワーカースレッド上で直接実行する (既定はハンドラ用プール)。 */
#define VEIL_ROUTE_INLINE 1u
/* 要求本文を溜めずにコールバックを呼ぶ (veil_request_body_read で順に読む。INLINE とは併用不可)。 */
#define VEIL_ROUTE_STREAM_BODY 2u
/*
 * コールバックの前に本文を読み、multipart/form-data はパートに分ける (F-266。STREAM_BODY とは併用不可)。
 * [api] multipart_spool_threshold を超えるファイルは名前の無い一時ファイル ([api] upload_tmp_dir) に書き、
 * veil_multipart_next の fd で渡す (本文全体をメモリに溜めない)。壊れた本文は 400、上限超えは 413 を
 * コールバックの前に返す。
 */
#define VEIL_ROUTE_FORM 4u
/* このルートの応答を [route.compression] / [api] compression で圧縮しない (F-270)。 */
#define VEIL_ROUTE_NO_COMPRESS 8u

/* ---- 不透明型 ---- */
typedef struct veil_config   veil_config_t;   /* 設定 + 登録待ちルート */
typedef struct veil_request  veil_request_t;  /* コールバックの間だけ有効 */
typedef struct veil_response veil_response_t; /* コールバックの間だけ有効 */

/*
 * ハンドラ。0 を返すと resp を送る。非 0 なら 500。
 * resp の既定は 200・空本文。
 */
typedef int (*veil_handler_fn)(const veil_request_t *req, veil_response_t *resp, void *user_data);

/* ---- 設定 ---- */

/* ライブラリ既定の設定を作る。veil_run か veil_config_free で解放する。 */
veil_config_t *veil_config_new(void);

/* config.toml を取り込んだ設定を作る。失敗したら NULL (veil_last_error に理由)。 */
veil_config_t *veil_config_from_file(const char *path);

/*
 * 任意のキーを上書きする。path は "server.threads" のようなドット区切り、
 * toml_value は TOML の値 (文字列は引用符込み: "\"warn\"")。
 */
int veil_config_set(veil_config_t *cfg, const char *path, const char *toml_value);

/* 平文の待ち受けアドレス ([server].listen_cleartext、HTTP/1.1 + h2c)。例: "127.0.0.1:8080" (既定。外部に公開するときは "0.0.0.0:8080") */
int veil_config_listen_cleartext(veil_config_t *cfg, const char *addr);

/* TLS の待ち受けアドレス ([server].listen、HTTP/1.1 + h2 ALPN)。veil_config_tls と併用する。 */
int veil_config_listen(veil_config_t *cfg, const char *addr);

/* TLS の証明書と秘密鍵 ([tls].cert_path / key_path)。 */
int veil_config_tls(veil_config_t *cfg, const char *cert_path, const char *key_path);

/* ワーカースレッド数 ([server].threads)。 */
int veil_config_threads(veil_config_t *cfg, uint32_t n);

/*
 * 待ち受け 1 個 ([[listener]]、F-260)。veil_config_add_listener に渡す。
 * 文字列は NULL なら省略 (既定)。http2・allow_http・allow_h2c・secure_redirect は負なら省略 (既定)、
 * 0 で false、正で true。threads は 0 なら省略 ([server].threads)。
 */
typedef struct {
    const char *name;                    /* ログ・redirect_to の名前 (NULL = "listener-<添字>") */
    const char *listen;                  /* 必須。"host:port" か "unix:<path>" */
    int tls;                             /* 0 = 平文 (HTTP/1.1 + h2c)、非 0 = TLS (veil_config_tls の証明書) */
    int http2;                           /* TLS: ALPN に h2 を出すか (既定 [server].http2_enabled) */
    int http3;                           /* TLS: 非 0 なら同じポートの UDP で HTTP/3 */
    const char *http3_listen;            /* HTTP/3 の UDP を別のアドレスにするとき */
    int allow_http;                      /* 平文: HTTP/1.1 を受けるか (既定 1) */
    int allow_h2c;                       /* 平文: h2c を受けるか (既定 1) */
    int secure_redirect;                 /* 平文: https へ 301 で返すか (既定 0) */
    const char *redirect_to;             /* 平文: リダイレクト先のポートを取る TLS のリスナー名 */
    uint32_t threads;                    /* このリスナーのワーカー数 */
    const char *unix_socket_permissions; /* unix:<path> のパーミッション ("0660" 等) */
} veil_listener_t;

/*
 * 待ち受けを 1 個足す ([[listener]])。ポートごとに 1 回呼ぶ。1 個でも足すと、ライブラリ既定の
 * 127.0.0.1:8080 (listen_cleartext) は veil_config_listen_cleartext で明示しない限り開かない。
 */
int veil_config_add_listener(veil_config_t *cfg, const veil_listener_t *listener);

/* 設定を解放する。veil_run に渡した後は呼ばない。NULL は何もしない。 */
void veil_config_free(veil_config_t *cfg);

/* ---- ルート登録 ---- */

/*
 * ルートを登録する。method が NULL・空・"*" なら全メソッド。コールバックはハンドラ用プールで実行。
 * user_data はサーバ稼働中ずっと有効で、複数スレッドから同時に使っても安全であること。
 * パスは "/users/{id}" のように {name} でパラメータを取れる。
 */
int veil_register_route(veil_config_t *cfg, const char *path, const char *method,
                        veil_handler_fn fn, void *user_data);

/*
 * 拡張版。methods_csv はカンマ区切り ("GET,POST")。flags は VEIL_ROUTE_* の OR。
 * openapi_json は OpenAPI の Operation Object の JSON (NULL 可)。/openapi.json に反映される。
 */
int veil_register_route_ex(veil_config_t *cfg, const char *path, const char *methods_csv,
                           veil_handler_fn fn, void *user_data, uint32_t flags,
                           const char *openapi_json);

/* ---- 起動・停止 ---- */

/*
 * サーバを起動して停止 (SIGINT / SIGTERM / veil_shutdown) までブロックする。
 * cfg は成功・失敗にかかわらず消費される。成功で 0、失敗で負のエラーコード。
 */
int veil_run(veil_config_t *cfg);

/* 実行中のサーバを graceful に停止する (別スレッドから呼べる)。 */
void veil_shutdown(void);

/* ---- リクエストのアクセサ ----
 *
 * 戻り値は NUL 終端ではない。長さは *len に入る (len は NULL 可)。
 * 見つからなければ NULL と長さ 0。ポインタはコールバックから戻るまで有効。
 */
const char *veil_request_method(const veil_request_t *req, size_t *len);
/* クエリを除いたパス (パーセントエンコードのまま)。 */
const char *veil_request_path(const veil_request_t *req, size_t *len);
/* "?" の後ろ。無ければ長さ 0。 */
const char *veil_request_query(const veil_request_t *req, size_t *len);
/* 本文 (バイナリ可)。 */
const char *veil_request_body(const veil_request_t *req, size_t *len);
/* ヘッダの値 (名前は大文字小文字を区別しない)。 */
const char *veil_request_header(const veil_request_t *req, const char *name, size_t *len);
/* パスパラメータ {name} の値 (パーセント復号済み)。 */
const char *veil_request_param(const veil_request_t *req, const char *name, size_t *len);
/* クエリパラメータの値 (最初の出現。+ と %XX を復号済み)。 */
const char *veil_request_query_param(const veil_request_t *req, const char *name, size_t *len);
/* 送信元 IP の文字列表現。 */
const char *veil_request_client_ip(const veil_request_t *req, size_t *len);

/* ---- multipart/form-data (F-266) ---- */

/* 1 パート。ポインタはコールバックの間だけ有効 (NUL 終端ではない)。 */
typedef struct {
    const char *name;          /* Content-Disposition の name (無ければ NULL) */
    size_t name_len;
    const char *filename;      /* filename (ファイルでなければ NULL。filename* は UTF-8 に復号済み) */
    size_t filename_len;
    const char *content_type;  /* パートの Content-Type (無ければ NULL) */
    size_t content_type_len;
    const char *data;          /* 中身 (一時ファイルに書いたときは NULL) */
    size_t data_len;
    intptr_t fd;               /* 一時ファイルの fd (Windows は HANDLE)。先頭に位置づけ済み。メモリなら -1。
                                  コールバックから戻ると veil が閉じる (残すなら dup する) */
    uint64_t size;             /* 中身のバイト数 */
} veil_part_t;

/*
 * 次のパートを *out に入れる。1 = 入れた、0 = 終わり。VEIL_ERR_FORM (400)・VEIL_ERR_TOO_LARGE (413)・
 * VEIL_ERR_STATE (multipart の本文ではない)。VEIL_ROUTE_FORM の無いルートは溜めた本文を解析する。
 */
int veil_multipart_next(const veil_request_t *req, veil_part_t *out);

/* ---- レスポンスのセッター (成功で 0、失敗で負のエラーコード) ---- */

/* ステータス (100-599)。 */
int veil_response_set_status(veil_response_t *resp, uint16_t status);
/* ヘッダを設定 (同名は置換)。CR/LF などの制御文字は拒否。 */
int veil_response_set_header(veil_response_t *resp, const char *name, const char *value);
/* 本文を設定 (コピーされる)。Content-Type は変えない。 */
int veil_response_set_body(veil_response_t *resp, const uint8_t *data, size_t len);
/* text/plain; charset=utf-8 の本文 (NUL 終端の UTF-8)。 */
int veil_response_set_text(veil_response_t *resp, const char *text);
/* application/json の本文 (呼び出し側が正しい JSON を渡す)。 */
int veil_response_set_json(veil_response_t *resp, const char *json);
/* この応答を圧縮しない (ルートが [route.compression] / [api] compression で圧縮するときも。F-270)。 */
int veil_response_no_compression(veil_response_t *resp);

/* ---- ストリーミング (F-210。ハンドラ用プールのルートだけ) ----
 *
 * veil_response_stream は resp に設定済みのステータスとヘッダを先に送り、本文を書くハンドルを返す。
 * ハンドルはコールバックから戻った後も別スレッドで使える。veil_stream_close で本文を終えて解放する
 * (閉じないと応答が終わらない)。INLINE のルート・2 回目は NULL。
 */
typedef struct veil_stream veil_stream_t;
veil_stream_t *veil_response_stream(veil_response_t *resp);
/* 本文の断片を送る (コピーする。満杯なら待つ)。切断されていたら VEIL_ERR_CLOSED。 */
int veil_stream_write(veil_stream_t *s, const uint8_t *data, size_t len);
/* 本文の後に送るトレーラを足す。 */
int veil_stream_set_trailer(veil_stream_t *s, const char *name, const char *value);
/* 本文を終えて (トレーラを送って) 解放する。NULL は何もしない。 */
void veil_stream_close(veil_stream_t *s);
/*
 * 要求本文の次の断片を buf へ読む。読んだバイト数、0 は終わり、負はエラー。届くまで待つ。
 * VEIL_ROUTE_STREAM_BODY のルートでは届いた順に、それ以外は全量の本文を順に返す。コールバックの中でだけ。
 */
int64_t veil_request_body_read(const veil_request_t *req, uint8_t *buf, size_t cap);

/* ---- gRPC (F-210) ----
 *
 * メッセージは符号化済みのバイト列 (protobuf-c・nanopb・upb 等で符号化・復号する)。コールバックは
 * ハンドラ用プールで呼ばれ、0 を返すと veil_grpc_set_status の状態 (未設定なら OK) で終える。非 0 は INTERNAL。
 * 何も送らずにエラーで終えると Trailers-Only の応答になる。
 */
#define VEIL_GRPC_UNARY            0u /* 1 要求 → 1 応答 */
#define VEIL_GRPC_SERVER_STREAMING 1u /* 1 要求 → 応答の列 */
#define VEIL_GRPC_CLIENT_STREAMING 2u /* 要求の列 → 1 応答 */
#define VEIL_GRPC_BIDI             3u /* 双方向 (全二重) */
typedef struct veil_grpc_call veil_grpc_call_t;
typedef int (*veil_grpc_fn)(veil_grpc_call_t *call, void *user_data);
/* path は "/<package>.<Service>/<Method>"。 */
int veil_register_grpc(veil_config_t *cfg, const char *path, uint32_t kind, veil_grpc_fn fn, void *user_data);
/* grpc.health.v1.Health (Check・Watch) を登録する。 */
int veil_register_grpc_health(veil_config_t *cfg);
/*
 * 次の受信メッセージ。1 = 受け取った (*data・*len は次の recv かコールバックの終わりまで有効)、0 = 終わり、
 * 負 = エラー (壊れた・大きすぎるメッセージ)。単項・サーバストリーミングは 1 通だけ返す。
 */
int veil_grpc_recv(veil_grpc_call_t *call, const uint8_t **data, size_t *len);
/* 送る (コピーする)。単項・クライアントストリーミングは 1 回だけ。切断されていたら VEIL_ERR_CLOSED。 */
int veil_grpc_send(veil_grpc_call_t *call, const uint8_t *data, size_t len);
/* 終えるときの状態 (0..16、message は NULL 可)。 */
int veil_grpc_set_status(veil_grpc_call_t *call, int code, const char *message);
/* 応答ヘッダ (最初の send より前だけ) と正常終了のトレーラ。 */
int veil_grpc_set_header(veil_grpc_call_t *call, const char *name, const char *value);
int veil_grpc_set_trailer(veil_grpc_call_t *call, const char *name, const char *value);
/* 要求メタデータの値 (大文字小文字を区別しない。NUL 終端ではない)。 */
const char *veil_grpc_metadata(const veil_grpc_call_t *call, const char *name, size_t *len);
/* 呼ばれたメソッド。 */
const char *veil_grpc_method(const veil_grpc_call_t *call, size_t *len);
/* 期限までの残りミリ秒 (期限なし -1、過ぎていれば 0)。 */
int64_t veil_grpc_deadline_ms(const veil_grpc_call_t *call);

/* ---- L4 (TCP/UDP、F-210) ----
 *
 * コールバックはハンドラ用プールで、接続ごとに 1 つずつ順に呼ばれる (同じ接続のコールバックは並行しない)。
 * veil_l4_write はコールバックの中でだけ呼べ、書いたデータはコールバックから戻った後に送られる。
 */
typedef struct veil_l4_conn veil_l4_conn_t;
typedef struct {
    int (*on_connect)(veil_l4_conn_t *conn, void *user_data);                              /* 非 0 で閉じる */
    int (*on_data)(veil_l4_conn_t *conn, const uint8_t *data, size_t len, void *user_data); /* 非 0 で閉じる */
    void (*on_close)(veil_l4_conn_t *conn, void *user_data);
} veil_l4_tcp_callbacks_t;
int veil_register_l4_tcp(veil_config_t *cfg, const char *name, const char *listen,
                         const veil_l4_tcp_callbacks_t *callbacks, void *user_data);
/*
 * HTTP/1.1 のプロトコルアップグレード (F-261): 応答を 101 Switching Protocols (Upgrade: protocol) にし、
 * 101 を書いた後の接続を上の L4 の TCP コールバックで受ける (on_connect → on_data … → on_close。
 * veil_l4_write・veil_l4_close が使える)。resp に設定済みのヘッダは 101 と一緒に送る。
 * 要求が Connection: upgrade + Upgrade を持たない・本文付き・HTTP/2・HTTP/3 なら 400 になる。h2c は予約 (500)。
 */
int veil_response_upgrade(veil_response_t *resp, const char *protocol, const veil_l4_tcp_callbacks_t *callbacks,
                          void *user_data);
int veil_l4_write(veil_l4_conn_t *conn, const uint8_t *data, size_t len);
void veil_l4_close(veil_l4_conn_t *conn);
const char *veil_l4_peer(const veil_l4_conn_t *conn, size_t *len);
void veil_l4_set_conn_data(veil_l4_conn_t *conn, void *data);
void *veil_l4_conn_data(const veil_l4_conn_t *conn);
typedef struct veil_l4_reply veil_l4_reply_t;
/* UDP: データグラムごとに呼ばれる。veil_l4_reply で返信する。 */
typedef int (*veil_l4_udp_fn)(const uint8_t *data, size_t len, const char *peer, veil_l4_reply_t *reply,
                              void *user_data);
int veil_register_l4_udp(veil_config_t *cfg, const char *name, const char *listen, veil_l4_udp_fn fn,
                         void *user_data);
int veil_l4_reply(veil_l4_reply_t *reply, const uint8_t *data, size_t len);

/* ---- WebSocket (RFC 6455、F-262) ----
 *
 * ハンドラの中で veil_ws_accept を呼ぶと応答が 101 になり、その後の接続をコールバックで受ける
 * (ハンドラ用プールで、接続ごとに 1 つずつ順に呼ばれる)。Ping への Pong・閉じる手順・断片の組み立て・
 * UTF-8 の検査は veil が行う。veil_ws_send・veil_ws_close は on_open から on_close が戻るまでの間なら
 * どのスレッドからでも呼べる (送信キューに積んで戻る)。HTTP/1.1 だけ (HTTP/2・HTTP/3 では 400)。
 */
#define VEIL_WS_TEXT   1
#define VEIL_WS_BINARY 2
typedef struct veil_ws veil_ws_t;
typedef struct {
    int (*on_open)(veil_ws_t *ws, void *user_data);                                          /* 非 0 で閉じる (on_close は呼ばれない) */
    int (*on_message)(veil_ws_t *ws, int opcode, const uint8_t *data, size_t len, void *user_data); /* 非 0 で閉じる (1000) */
    void (*on_close)(veil_ws_t *ws, uint16_t code, void *user_data); /* 相手の Close のコード (無ければ 1005、切断は 1006) */
} veil_ws_callbacks_t;
/* 0 = 受け入れた (resp は 101)。VEIL_ERR_STATE = ハンドシェイクが正しくない (resp は 400 か 426。そのまま返してよい)。 */
int veil_ws_accept(const veil_request_t *req, veil_response_t *resp, const veil_ws_callbacks_t *callbacks,
                   void *user_data);
/* 送る (コピーする)。VEIL_ERR_CLOSED = 閉じた後、VEIL_ERR_STATE = 送信キューが一杯 (16 MiB)、VEIL_ERR_UTF8 = テキストが UTF-8 でない。 */
int veil_ws_send(veil_ws_t *ws, int opcode, const uint8_t *data, size_t len);
/* code の Close を送って閉じる手順を始める。 */
int veil_ws_close(veil_ws_t *ws, uint16_t code);
const char *veil_ws_peer(const veil_ws_t *ws, size_t *len);
void veil_ws_set_data(veil_ws_t *ws, void *data);
void *veil_ws_data(const veil_ws_t *ws);

/* ---- その他 ---- */

/* 呼び出しスレッドで最後に起きたエラーのメッセージ (NUL 終端)。無ければ NULL。 */
const char *veil_last_error(void);
/* ライブラリのバージョン (NUL 終端、静的)。 */
const char *veil_version(void);

#ifdef __cplusplus
}
#endif

#endif /* VEIL_H */
