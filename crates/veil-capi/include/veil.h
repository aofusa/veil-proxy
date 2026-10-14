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

/* ---- ルートのフラグ ---- */
/* コールバックをワーカースレッド上で直接実行する (既定はハンドラ用プール)。 */
#define VEIL_ROUTE_INLINE 1u
/* 要求本文を溜めずにコールバックを呼ぶ (veil_request_body_read で順に読む。INLINE とは併用不可)。 */
#define VEIL_ROUTE_STREAM_BODY 2u

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

/* ---- その他 ---- */

/* 呼び出しスレッドで最後に起きたエラーのメッセージ (NUL 終端)。無ければ NULL。 */
const char *veil_last_error(void);
/* ライブラリのバージョン (NUL 終端、静的)。 */
const char *veil_version(void);

#ifdef __cplusplus
}
#endif

#endif /* VEIL_H */
