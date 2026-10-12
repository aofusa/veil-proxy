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

/* ---- ルートのフラグ ---- */
/* コールバックをワーカースレッド上で直接実行する (既定はハンドラ用プール)。 */
#define VEIL_ROUTE_INLINE 1u

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

/* 待ち受けアドレス ([server].listen)。例: "0.0.0.0:8080" */
int veil_config_listen(veil_config_t *cfg, const char *addr);

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

/* ---- その他 ---- */

/* 呼び出しスレッドで最後に起きたエラーのメッセージ (NUL 終端)。無ければ NULL。 */
const char *veil_last_error(void);
/* ライブラリのバージョン (NUL 終端、静的)。 */
const char *veil_version(void);

#ifdef __cplusplus
}
#endif

#endif /* VEIL_H */
