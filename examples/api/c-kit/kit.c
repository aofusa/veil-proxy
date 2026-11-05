/*
 * veil C helpers kit のサンプル（F-291）。libveil_kit とリンクする（libveil とは一緒にリンクしない）。
 *
 * 環境変数: VEIL_LISTEN（既定 127.0.0.1:8080）、VEIL_STATIC_DIR（/static で出すディレクトリ）、
 * VEIL_DOWNLOAD（/download で返すファイル）、VEIL_SHUTDOWN_MARK（停止後に書くファイル）。
 * Cookie の鍵と JWT の秘密は examples/api/rust-kit と同じ（Rust と C のサービスが互いの Cookie・トークンを読める）。
 */
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "veil_kit.h"

static const char COOKIE_SECRET[] = "kit-example-cookie-secret-0123456789";
static const char JWT_SECRET[] = "kit-example-secret-0123456789abcdef";

static veil_kit_key_t *g_key;
static veil_kit_jwt_verifier_t *g_jwt;
static atomic_long g_background, g_ticks, g_cron, g_started, g_workers;

/* ---- ミドルウェア: 要求 ID を要求ごとの値に置き、後処理でヘッダに出す ---- */

static void free_str(void *p) { free(p); }

static int mw_before(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)resp;
    (void)ud;
    size_t len = 0;
    const char *id = veil_request_header(req, "x-request-id", &len);
    char *copy = malloc(len + 1);
    if (copy == NULL) return -1;
    memcpy(copy, id ? id : "", len);
    copy[len] = '\0';
    return veil_request_set_local(req, "request-id", copy, free_str);
}

static void mw_after(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    const char *id = veil_request_get_local(req, "request-id");
    if (id != NULL && id[0] != '\0') veil_response_set_header(resp, "x-request-id", id);
    veil_response_add_header(resp, "x-mw", "after");
}

/* /mw/stop は前処理で打ち切る */
static int mw_stop(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    size_t len = 0;
    const char *p = veil_request_path(req, &len);
    if (len == 8 && memcmp(p, "/mw/stop", 8) == 0) {
        veil_response_set_status(resp, 418);
        veil_response_set_text(resp, "stopped by middleware");
        return VEIL_MW_STOP;
    }
    return VEIL_MW_CONTINUE;
}

static int mw_echo(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    const char *id = veil_request_get_local(req, "request-id");
    char buf[160];
    snprintf(buf, sizeof buf, "local=%s", id ? id : "(none)");
    return veil_response_set_text(resp, buf);
}

/* ---- ヘッダ・プロトコル ---- */

static int headers(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    char buf[512];
    size_t off = 0, it = 0, len = 0;
    const char *v;
    off += (size_t)snprintf(buf, sizeof buf, "n=%zu multi=", veil_request_header_count(req));
    while ((v = veil_request_header_next(req, "x-multi", &it, &len)) != NULL && off < sizeof buf - 64)
        off += (size_t)snprintf(buf + off, sizeof buf - off, "%.*s;", (int)len, v);
    snprintf(buf + off, sizeof buf - off, " version=%d tls=%d", veil_request_version(req), veil_request_is_tls(req));
    /* 同名のヘッダを 2 個 */
    veil_response_add_header(resp, "x-twice", "1");
    veil_response_add_header(resp, "x-twice", "2");
    return veil_response_set_text(resp, buf);
}

/* ---- Cookie ---- */

static int cookie_visit(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    size_t len = 0;
    const char *v = veil_kit_cookie_get(req, "visits", &len);
    int n = 0;
    if (v != NULL && len < 10) {
        char tmp[11];
        memcpy(tmp, v, len);
        tmp[len] = '\0';
        n = atoi(tmp);
    }
    char val[16], body[32];
    snprintf(val, sizeof val, "%d", n + 1);
    snprintf(body, sizeof body, "visit %d", n + 1);
    veil_kit_cookie_t c = VEIL_KIT_COOKIE_INIT;
    c.name = "visits";
    c.value = val;
    c.http_only = 1;
    veil_kit_set_cookie(resp, &c);
    c.name = "theme";
    c.value = "dark";
    c.same_site = VEIL_KIT_SAMESITE_LAX;
    veil_kit_set_cookie(resp, &c);
    return veil_response_set_text(resp, body);
}

static int signed_set(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    veil_kit_cookie_t c = VEIL_KIT_COOKIE_INIT;
    c.name = "user";
    c.value = "alice";
    if (veil_kit_signed_set(resp, g_key, &c) != VEIL_OK) return 1;
    c.name = "secret";
    c.value = "s3cr3t";
    if (veil_kit_private_set(resp, g_key, &c) != VEIL_OK) return 1;
    return veil_response_set_text(resp, "set");
}

static int signed_get(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    size_t ul = 0, sl = 0;
    const char *u = veil_kit_signed_get(req, g_key, "user", &ul);
    const char *s = veil_kit_private_get(req, g_key, "secret", &sl);
    char buf[256];
    snprintf(buf, sizeof buf, "user=%.*s secret=%.*s", (int)(u ? ul : 7), u ? u : "invalid", (int)(s ? sl : 7),
             s ? s : "invalid");
    return veil_response_set_text(resp, buf);
}

/* ---- セッション ---- */

static int session_login(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    size_t len = 0;
    const char *b = veil_request_body(req, &len);
    char name[64];
    if (len == 0 || len >= sizeof name) return 1;
    memcpy(name, b, len);
    name[len] = '\0';
    veil_kit_session_set_str(req, "user", name);
    veil_kit_session_cycle_id(req);
    return veil_response_set_text(resp, "logged in");
}

static int session_whoami(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    size_t ul = 0, nl = 0;
    const char *u = veil_kit_session_get_str(req, "user", &ul);
    const char *n = veil_kit_session_get(req, "n", &nl);
    long count = 1;
    if (n != NULL && nl < 20) {
        char tmp[21];
        memcpy(tmp, n, nl);
        tmp[nl] = '\0';
        count = atol(tmp) + 1;
    }
    char num[24], buf[128];
    snprintf(num, sizeof num, "%ld", count);
    veil_kit_session_set(req, "n", num);
    snprintf(buf, sizeof buf, "%.*s %ld", (int)(u ? ul : 9), u ? u : "anonymous", count);
    return veil_response_set_text(resp, buf);
}

static int session_logout(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    veil_kit_session_destroy(req);
    return veil_response_set_text(resp, "bye");
}

/* ---- 認証 ---- */

static int auth_basic(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    if (!veil_kit_basic_matches(req, "admin", "pw")) return veil_kit_unauthorized(resp, "Basic realm=\"veil\"");
    return veil_response_set_text(resp, "basic admin");
}

static int auth_key(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    static const char *const keys[] = {"k-123", "k-456"};
    size_t len = 0;
    if (veil_kit_api_key(req, NULL, &len) == NULL) return veil_kit_error(resp, 401, "missing API key");
    if (!veil_kit_api_key_matches(req, NULL, keys, 2)) return veil_kit_error(resp, 403, "wrong API key");
    return veil_response_set_text(resp, "key ok");
}

/* POST /auth/token: {"user": ..., "admin": bool} の代わりに form（grant_type=password の形）で受ける */
static int auth_token(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    size_t ul = 0, al = 0;
    const char *u = veil_kit_form_get(req, "username", &ul);
    const char *a = veil_kit_form_get(req, "scope", &al);
    if (u == NULL || ul == 0 || ul > 64) return veil_kit_error(resp, 422, "username is required");
    char user[65], scope[65] = "read";
    memcpy(user, u, ul);
    user[ul] = '\0';
    if (a != NULL && al > 0 && al < sizeof scope) {
        memcpy(scope, a, al);
        scope[al] = '\0';
    }
    char *qu = veil_kit_json_string(user), *qs = veil_kit_json_string(scope);
    char claims[512];
    snprintf(claims, sizeof claims, "{\"sub\":%s,\"iss\":\"veil-kit-example\",\"exp\":%lld,\"scope\":%s}", qu,
             (long long)time(NULL) + 300, qs);
    veil_kit_free(qu);
    veil_kit_free(qs);
    char *tok = NULL;
    if (veil_kit_jwt_encode_hmac("HS256", (const uint8_t *)JWT_SECRET, strlen(JWT_SECRET), NULL, claims, &tok) != VEIL_OK)
        return 1;
    int rc = veil_response_set_text(resp, tok);
    veil_kit_free(tok);
    return rc;
}

static int auth_me(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    const veil_kit_principal_t *p = veil_kit_principal(req);
    if (p == NULL) return veil_kit_unauthorized(resp, NULL);
    size_t len = 0;
    const char *sub = veil_kit_principal_subject(p, &len);
    char buf[128];
    snprintf(buf, sizeof buf, "me %.*s", (int)len, sub);
    return veil_response_set_text(resp, buf);
}

static int admin_area(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    size_t len = 0;
    const char *sub = veil_kit_principal_subject(veil_kit_principal(req), &len);
    char buf[128];
    snprintf(buf, sizeof buf, "admin %.*s", (int)len, sub);
    return veil_response_set_text(resp, buf);
}

/* POST /auth/verify: 本文のトークンを検証して claims の JSON を返す（他の実装が発行したトークンの確認用） */
static int auth_verify(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    size_t len = 0;
    const char *t = veil_request_body(req, &len);
    char *claims = NULL;
    if (veil_kit_jwt_verify(g_jwt, t, len, &claims) != VEIL_OK) return veil_kit_error(resp, 401, veil_last_error());
    int rc = veil_response_set_json(resp, claims);
    veil_kit_free(claims);
    return rc;
}

/* ---- リダイレクト・ファイル ---- */

static int redirect(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    static const char *const hosts[] = {"app.example.com"};
    size_t len = 0;
    const char *next = veil_request_query_param(req, "next", &len);
    char target[256] = "/";
    if (next != NULL && len < sizeof target) {
        memcpy(target, next, len);
        target[len] = '\0';
    }
    if (veil_kit_redirect_safe(resp, 0, target, hosts, 1) != VEIL_OK) return veil_kit_redirect(resp, 0, "/");
    return VEIL_OK;
}

static int download(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    const char *path = getenv("VEIL_DOWNLOAD");
    if (path == NULL) return veil_kit_error(resp, 404, "no download configured");
    return veil_kit_send_file(resp, path, 1 << 20);
}

/* ---- 応答後の処理・タスク・ライフサイクル ---- */

static void background(void *ud) {
    (void)ud;
    atomic_fetch_add(&g_background, 1);
}

static int tasks_enqueue(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    veil_response_after_send(resp, background, NULL);
    return veil_response_set_text(resp, "queued");
}

static int tasks_status(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    char buf[256];
    snprintf(buf, sizeof buf, "{\"background\":%ld,\"ticks\":%ld,\"cron\":%ld,\"started\":%ld,\"workers\":%ld}",
             atomic_load(&g_background), atomic_load(&g_ticks), atomic_load(&g_cron), atomic_load(&g_started),
             atomic_load(&g_workers));
    return veil_response_set_json(resp, buf);
}

static void tick(void *ud) { atomic_fetch_add((atomic_long *)ud, 1); }

static int on_startup(void *ud) {
    (void)ud;
    atomic_fetch_add(&g_started, 1);
    return 0;
}

static void on_worker(const veil_worker_info_t *info, void *ud) {
    (void)info;
    (void)ud;
    atomic_fetch_add(&g_workers, 1);
}

static void on_shutdown(void *ud) {
    (void)ud;
    const char *mark = getenv("VEIL_SHUTDOWN_MARK");
    if (mark != NULL) {
        FILE *f = fopen(mark, "w");
        if (f != NULL) {
            fprintf(f, "background=%ld\n", atomic_load(&g_background));
            fclose(f);
        }
    }
}

/* ---- WebSocket（サブプロトコルの選択） ---- */

static int ws_open(veil_ws_t *ws, void *ud) {
    (void)ud;
    size_t len = 0;
    const char *p = veil_ws_protocol(ws, &len);
    char buf[64];
    int n = snprintf(buf, sizeof buf, "protocol=%.*s", (int)(p ? len : 4), p ? p : "none");
    return veil_ws_send(ws, VEIL_WS_TEXT, (const uint8_t *)buf, (size_t)n);
}

static int ws_message(veil_ws_t *ws, int opcode, const uint8_t *data, size_t len, void *ud) {
    (void)ud;
    return veil_ws_send(ws, opcode, data, len);
}

static int ws_route(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    static const veil_ws_callbacks_t cb = {ws_open, ws_message, NULL};
    veil_ws_options_t o = VEIL_WS_OPTIONS_INIT;
    o.protocols = "graphql-transport-ws, chat";
    o.origins = getenv("VEIL_WS_ORIGIN");
    int rc = veil_ws_accept_ex(req, resp, &cb, NULL, &o);
    return rc == VEIL_ERR_STATE ? VEIL_OK : rc; /* resp は 400 / 403 */
}

static int hello(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    return veil_response_set_text(resp, "hello");
}

static const veil_kit_route_t ROUTES[] = {
    VEIL_KIT_GET("/api/hello", hello),
    VEIL_KIT_POST("/api/echo", hello),
    VEIL_KIT_GET("/mw/echo", mw_echo),
    VEIL_KIT_GET("/mw/stop", mw_echo),
    VEIL_KIT_GET("/headers", headers),
    VEIL_KIT_GET("/cookie/visit", cookie_visit),
    VEIL_KIT_POST("/cookie/signed", signed_set),
    VEIL_KIT_GET("/cookie/signed", signed_get),
    VEIL_KIT_POST("/session/login", session_login),
    VEIL_KIT_GET("/session/whoami", session_whoami),
    VEIL_KIT_POST("/session/logout", session_logout),
    VEIL_KIT_GET("/auth/basic", auth_basic),
    VEIL_KIT_GET("/auth/key", auth_key),
    VEIL_KIT_POST("/auth/token", auth_token),
    VEIL_KIT_GET("/auth/me", auth_me),
    VEIL_KIT_POST("/auth/verify", auth_verify),
    VEIL_KIT_GET("/admin/area", admin_area),
    VEIL_KIT_GET("/redirect", redirect),
    VEIL_KIT_GET("/download", download),
    VEIL_KIT_POST("/tasks/enqueue", tasks_enqueue),
    VEIL_KIT_GET("/tasks/status", tasks_status),
    VEIL_KIT_GET("/ws", ws_route),
};

#define CHECK(x)                                                                       \
    do {                                                                               \
        if ((x) != VEIL_OK) {                                                          \
            fprintf(stderr, "%s failed: %s\n", #x, veil_last_error() ? veil_last_error() : "?"); \
            return 1;                                                                  \
        }                                                                              \
    } while (0)

int main(void) {
    printf("veil kit %s\n", veil_kit_version());
    g_key = veil_kit_key_derive((const uint8_t *)COOKIE_SECRET, strlen(COOKIE_SECRET));
    g_jwt = veil_kit_jwt_verifier_new();
    if (g_key == NULL || g_jwt == NULL) return 1;
    CHECK(veil_kit_jwt_verifier_add_hmac(g_jwt, "HS256", (const uint8_t *)JWT_SECRET, strlen(JWT_SECRET), NULL));
    CHECK(veil_kit_jwt_verifier_issuer(g_jwt, "veil-kit-example"));

    veil_config_t *cfg = veil_config_new();
    const char *listen = getenv("VEIL_LISTEN");
    CHECK(veil_config_listen_cleartext(cfg, listen ? listen : "127.0.0.1:8080"));
    CHECK(veil_config_set(cfg, "access_log.enabled", "false"));
    CHECK(veil_kit_register_all(cfg, ROUTES, sizeof ROUTES / sizeof ROUTES[0], NULL));

    /* CORS（/api だけ）と静的ディレクトリ */
    static const char *const origins[] = {"https://app.example.com", "https://*.example.org"};
    veil_cors_t cors = VEIL_CORS_INIT;
    cors.prefix = "/api";
    cors.origins = origins;
    cors.n_origins = 2;
    cors.credentials = 1;
    CHECK(veil_config_cors(cfg, &cors));
    const char *dir = getenv("VEIL_STATIC_DIR");
    if (dir != NULL) {
        veil_serve_options_t so = VEIL_SERVE_OPTIONS_INIT;
        so.index = "index.html";
        CHECK(veil_config_serve_dir(cfg, "/static", dir, &so));
    }

    /* ミドルウェア（後に登録したものほど外側） */
    CHECK(veil_register_middleware(cfg, "/mw", NULL, mw_after, NULL));
    CHECK(veil_register_middleware(cfg, "/mw", mw_before, NULL, NULL));
    CHECK(veil_register_middleware(cfg, "/mw/stop", mw_stop, NULL, NULL));

    /* 認証: require を先に、JWT の authenticate を後に登録する（後のものが先に動く） */
    CHECK(veil_kit_require_scope(cfg, "/admin", "admin"));
    CHECK(veil_kit_jwt_authenticate(cfg, "/admin", g_jwt, 0));
    CHECK(veil_kit_jwt_authenticate(cfg, "/auth/me", g_jwt, 0));

    /* セッション（/session、メモリに置き Cookie には署名した ID） */
    veil_kit_sessions_t so = VEIL_KIT_SESSIONS_INIT;
    so.prefix = "/session";
    so.secure = VEIL_OFF;
    so.memory_store = 1;
    CHECK(veil_kit_sessions_install(cfg, g_key, &so));

    /* タスクとライフサイクル */
    CHECK(veil_kit_every(cfg, 200, tick, &g_ticks));
    CHECK(veil_kit_cron(cfg, "* * * * * *", tick, &g_cron));
    CHECK(veil_config_on_startup(cfg, on_startup, NULL));
    CHECK(veil_config_on_worker_start(cfg, on_worker, NULL));
    CHECK(veil_config_on_shutdown(cfg, on_shutdown, NULL));

    int rc = veil_run(cfg);
    veil_kit_jwt_verifier_free(g_jwt);
    veil_kit_key_free(g_key);
    if (rc != VEIL_OK) {
        fprintf(stderr, "veil_run: %s\n", veil_last_error() ? veil_last_error() : "?");
        return 1;
    }
    return 0;
}
