/* veil C API のサンプル。ビルドは Makefile を参照。 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "veil.h"

/* GET / : プールで実行される通常のルート */
static int hello(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    return veil_response_set_text(resp, "Hello from C");
}

/* GET /users/{id} : パスパラメータを JSON にして返す */
static int get_user(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    size_t len = 0;
    const char *id = veil_request_param(req, "id", &len);
    if (id == NULL) {
        return 1;
    }
    char json[256];
    /* id は NUL 終端ではないので %.*s で長さを指定する。JSON の特殊文字は簡易に拒否する。 */
    for (size_t i = 0; i < len; i++) {
        if (id[i] == '"' || id[i] == '\\' || (unsigned char)id[i] < 0x20) {
            veil_response_set_status(resp, 400);
            return veil_response_set_json(resp, "{\"detail\":\"invalid id\"}");
        }
    }
    int n = snprintf(json, sizeof json, "{\"id\":\"%.*s\"}", (int)(len > 200 ? 200 : len), id);
    if (n < 0 || (size_t)n >= sizeof json) {
        return 1;
    }
    return veil_response_set_json(resp, json);
}

/* POST /echo : 本文をそのまま返す */
static int echo(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    size_t len = 0;
    const char *body = veil_request_body(req, &len);
    size_t ctlen = 0;
    const char *ct = veil_request_header(req, "content-type", &ctlen);
    if (ct != NULL && ctlen < 128) {
        char buf[128];
        memcpy(buf, ct, ctlen);
        buf[ctlen] = '\0';
        veil_response_set_header(resp, "content-type", buf);
    }
    return veil_response_set_body(resp, (const uint8_t *)body, len);
}

/* GET /ping : ワーカースレッドで直接実行する (短時間で終わる処理向け) */
static int ping(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    return veil_response_set_text(resp, "pong");
}

static int check(int rc, const char *what) {
    if (rc != VEIL_OK) {
        const char *e = veil_last_error();
        fprintf(stderr, "%s failed (%d): %s\n", what, rc, e ? e : "?");
        return 1;
    }
    return 0;
}

int main(int argc, char **argv) {
    veil_config_t *cfg = argc > 1 ? veil_config_from_file(argv[1]) : veil_config_new();
    if (cfg == NULL) {
        const char *e = veil_last_error();
        fprintf(stderr, "config failed: %s\n", e ? e : "?");
        return 1;
    }
    const char *listen = getenv("VEIL_LISTEN");
    if (listen != NULL && *listen != '\0' && check(veil_config_listen(cfg, listen), "listen")) {
        return 1;
    }

    const char *user_meta =
        "{\"summary\":\"Get a user\",\"tags\":[\"users\"],"
        "\"responses\":{\"200\":{\"description\":\"the user\"}}}";

    if (check(veil_register_route(cfg, "/", "GET", hello, NULL), "route /") ||
        check(veil_register_route_ex(cfg, "/users/{id}", "GET", get_user, NULL, 0, user_meta),
              "route /users/{id}") ||
        check(veil_register_route(cfg, "/echo", "POST", echo, NULL), "route /echo") ||
        check(veil_register_route_ex(cfg, "/ping", "GET", ping, NULL, VEIL_ROUTE_INLINE, NULL),
              "route /ping")) {
        veil_config_free(cfg);
        return 1;
    }

    printf("veil %s\n", veil_version());
    /* veil_run は cfg を消費する。SIGINT / SIGTERM で戻る。 */
    int rc = veil_run(cfg);
    if (check(rc, "veil_run")) {
        return 1;
    }
    return 0;
}
