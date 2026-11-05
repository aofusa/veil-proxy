/*
 * veil C helpers kit のサンプル（F-291）。libveil_kit とリンクする（libveil とは一緒にリンクしない）。
 *
 * 環境変数: VEIL_LISTEN（既定 127.0.0.1:8080）、VEIL_STATIC_DIR（/static で出すディレクトリ）、
 * VEIL_DOWNLOAD（/download で返すファイル）、VEIL_SHUTDOWN_MARK（停止後に書くファイル）。
 * `kit --selftest` はソケットを開かずにルートを呼ぶ試験（veil_test_*・veil_kit_test_*）だけを行う。
 * Cookie の鍵と JWT の秘密は examples/api/rust-kit と同じ（Rust と C のサービスが互いの Cookie・トークンを読める）。
 */
#include <pthread.h>
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

/* `hay` の `n` バイトに `needle` があるか（memmem は POSIX に無い） */
static int contains(const void *hay, size_t n, const char *needle) {
    size_t k = strlen(needle);
    const char *h = hay;
    for (size_t i = 0; h != NULL && i + k <= n; i++)
        if (memcmp(h + i, needle, k) == 0) return 1;
    return 0;
}

/* ---- HTTP クライアント（自分自身へ）・遅延応答 ---- */

static veil_client_t *g_client;
static char g_self[128]; /* http://127.0.0.1:<port> */
static atomic_long g_flaky;

static int body_text(veil_response_t *resp, veil_client_response_t *r) {
    size_t n = 0;
    const uint8_t *b = veil_client_response_body(r, &n);
    char buf[256];
    snprintf(buf, sizeof buf, "%d %.*s", veil_client_response_status(r), (int)n, (const char *)b);
    veil_client_response_free(r);
    return veil_response_set_text(resp, buf);
}

static int client_sync(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    char url[192];
    snprintf(url, sizeof url, "%s/api/hello", g_self);
    veil_client_request_t r = VEIL_CLIENT_REQUEST_INIT;
    r.url = url;
    veil_client_response_t *out = NULL;
    int rc = veil_client_send(g_client, &r, &out);
    if (rc != VEIL_OK) return veil_kit_error(resp, 502, veil_last_error());
    return body_text(resp, out);
}

/* INLINE のルート（ワーカー）では同期の呼び出しは VEIL_ERR_STATE */
static int client_inline(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    veil_client_request_t r = VEIL_CLIENT_REQUEST_INIT;
    r.url = g_self;
    veil_client_response_t *out = NULL;
    int rc = veil_client_send(g_client, &r, &out);
    return veil_response_set_text(resp, rc == VEIL_ERR_STATE ? "state" : "unexpected");
}

static int json_doc(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    return veil_response_set_json(resp, "{\"items\":[1,2,3]}");
}

static int client_json(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    char url[192];
    snprintf(url, sizeof url, "%s/json", g_self);
    veil_client_response_t *out = NULL;
    int ok = veil_kit_client_get_json(g_client, url, &out);
    snprintf(url, sizeof url, "%s/api/hello", g_self);
    veil_client_response_t *out2 = NULL;
    int notjson = veil_kit_client_get_json(g_client, url, &out2);
    veil_client_response_free(out2);
    char buf[256];
    size_t n = 0;
    const uint8_t *b = out ? veil_client_response_body(out, &n) : NULL;
    snprintf(buf, sizeof buf, "ok=%d notjson=%d body=%.*s", ok, notjson == VEIL_KIT_ERR_JSON, (int)n,
             b ? (const char *)b : "");
    veil_client_response_free(out);
    return veil_response_set_text(resp, buf);
}

/* 2 回 503 を返してから 200 */
static int flaky(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    if (atomic_fetch_add(&g_flaky, 1) % 3 < 2) {
        veil_response_set_status(resp, 503);
        return veil_response_set_text(resp, "busy");
    }
    return veil_response_set_text(resp, "recovered");
}

static int client_retry(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    atomic_store(&g_flaky, 0);
    char url[192];
    snprintf(url, sizeof url, "%s/flaky", g_self);
    veil_client_request_t r = VEIL_CLIENT_REQUEST_INIT;
    r.url = url;
    veil_kit_retry_t p = VEIL_KIT_RETRY_INIT;
    p.backoff_ms = 10;
    veil_client_response_t *out = NULL;
    if (veil_kit_client_send_retry(g_client, &r, &p, &out) != VEIL_OK) return veil_kit_error(resp, 502, veil_last_error());
    return body_text(resp, out);
}

/* 非同期の呼び出しの完了で遅延応答を返す（プールのスレッドを握らない） */
static void client_done(int rc, veil_client_response_t *r, void *ud) {
    veil_deferred_t *d = ud;
    veil_response_t *resp = veil_deferred_response(d);
    if (rc != VEIL_OK) {
        veil_kit_error(resp, 502, veil_last_error());
    } else {
        body_text(resp, r);
        veil_response_set_header(resp, "x-deferred", "1");
    }
    veil_deferred_complete(d);
}

static int client_async(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    char url[192];
    snprintf(url, sizeof url, "%s/api/hello", g_self);
    veil_deferred_t *d = veil_response_defer(resp);
    if (d == NULL) return -1;
    veil_client_request_t r = VEIL_CLIENT_REQUEST_INIT;
    r.url = url;
    if (veil_client_send_async(g_client, &r, client_done, d) != VEIL_OK) veil_deferred_free(d);
    return 0;
}

static void *deferred_thread(void *arg) {
    veil_deferred_t *d = arg;
    struct timespec ts = {0, 20 * 1000 * 1000};
    nanosleep(&ts, NULL);
    veil_response_set_text(veil_deferred_response(d), "later");
    veil_deferred_complete(d);
    return NULL;
}

static int deferred(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    veil_deferred_t *d = veil_response_defer(resp);
    if (d == NULL) return -1;
    pthread_t t;
    if (pthread_create(&t, NULL, deferred_thread, d) != 0) {
        veil_deferred_free(d);
        return 0;
    }
    pthread_detach(t);
    return 0;
}

/* ---- JSON 本文の検証（422） ---- */

static const char USER_SCHEMA[] =
    "{\"type\":\"object\",\"required\":[\"name\",\"age\"],\"additionalProperties\":false,"
    "\"properties\":{\"name\":{\"type\":\"string\",\"minLength\":1},"
    "\"age\":{\"type\":\"integer\",\"minimum\":0},\"email\":{\"type\":\"string\",\"format\":\"email\"}}}";

static int create_user(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    size_t n = 0;
    const char *b = veil_request_body(req, &n);
    veil_response_set_status(resp, 201);
    veil_response_set_header(resp, "content-type", "application/json");
    return veil_response_set_body(resp, (const uint8_t *)b, n);
}

/* ---- gRPC（サーバ・チャネル・Balanced） ---- */

static int grpc_echo(veil_grpc_call_t *call, void *ud) {
    (void)ud;
    const uint8_t *data = NULL;
    size_t len = 0;
    if (veil_grpc_recv(call, &data, &len) != 1) return veil_grpc_set_status(call, 3, "no message");
    veil_grpc_set_trailer(call, "x-echo", "yes");
    return veil_grpc_send(call, data, len);
}

static veil_grpc_channel_t *g_chan;
static veil_kit_grpc_balanced_t *g_lb;

static int grpc_reply(veil_response_t *resp, veil_grpc_client_call_t *c) {
    const uint8_t *m = NULL;
    size_t n = 0;
    int got = veil_grpc_client_recv(c, &m, &n);
    int code = veil_grpc_client_finish(c);
    size_t tl = 0;
    const char *t = veil_grpc_client_metadata(c, "x-echo", &tl);
    char buf[256];
    snprintf(buf, sizeof buf, "code=%d msg=%.*s trailer=%.*s", code, got == 1 ? (int)n : 0,
             got == 1 ? (const char *)m : "", (int)tl, t ? t : "");
    veil_grpc_client_call_free(c);
    return veil_response_set_text(resp, buf);
}

static int grpc_unary(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    veil_grpc_client_call_t *c = NULL;
    if (veil_grpc_channel_unary(g_chan, "/kit.Echo/Echo", (const uint8_t *)"ping", 4, NULL, &c) != VEIL_OK)
        return veil_kit_error(resp, 502, veil_last_error());
    return grpc_reply(resp, c);
}

static int grpc_balanced(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    /* 1 台目は落ちている（UNAVAILABLE）: どちらの順で回っても成功する */
    veil_grpc_client_call_t *c = NULL;
    for (int i = 0; i < 2; i++) {
        if (c != NULL) veil_grpc_client_call_free(c);
        c = NULL;
        if (veil_kit_grpc_balanced_unary(g_lb, "/kit.Echo/Echo", (const uint8_t *)"lb", 2, NULL, &c) != VEIL_OK)
            return veil_kit_error(resp, 502, veil_last_error());
        if (veil_grpc_client_finish(c) != 0) break;
    }
    return grpc_reply(resp, c);
}

/* ---- DI ---- */

static atomic_long g_made;

static void *make_counter(veil_kit_di_t *di, const veil_request_t *req, void *ud) {
    (void)di;
    (void)req;
    (void)ud;
    long *p = malloc(sizeof *p);
    if (p) *p = atomic_fetch_add(&g_made, 1) + 1;
    return p;
}

static veil_kit_di_t *g_di;

static int di_route(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    long *config = veil_kit_di_get(g_di, NULL, "config");
    long *s1 = veil_kit_di_get(g_di, req, "scoped");
    long *s2 = veil_kit_di_get(g_di, req, "scoped");
    long *t1 = veil_kit_di_get(g_di, req, "transient");
    long *t2 = veil_kit_di_get(g_di, req, "transient");
    if (!config || !s1 || !s2 || !t1 || !t2) return veil_kit_error(resp, 500, veil_last_error());
    veil_kit_jw_t w = VEIL_KIT_JW_INIT;
    veil_kit_jw_object(&w);
    veil_kit_jw_key(&w, "config");
    veil_kit_jw_int(&w, *config);
    veil_kit_jw_key(&w, "scoped_same");
    veil_kit_jw_bool(&w, s1 == s2);
    veil_kit_jw_key(&w, "transient_same");
    veil_kit_jw_bool(&w, t1 == t2);
    veil_kit_jw_key(&w, "note");
    veil_kit_jw_string(&w, "a \"quoted\"\n line");
    veil_kit_jw_end(&w);
    int rc = w.failed ? -1 : veil_response_set_json(resp, w.buf);
    free(w.buf);
    return rc;
}

/* ---- GraphQL（実行器はおもちゃ: hello と bump だけ） ---- */

static atomic_long g_bumps;

static int gql_exec(veil_kit_graphql_op_t *op, void *ud) {
    (void)ud;
    size_t n = 0;
    const char *q = veil_kit_graphql_query(op, &n);
    char buf[128];
    if (contains(q, n, "bump")) {
        snprintf(buf, sizeof buf, "{\"data\":{\"bump\":%ld}}", atomic_fetch_add(&g_bumps, 1) + 1);
    } else if (contains(q, n, "hello")) {
        snprintf(buf, sizeof buf, "{\"data\":{\"hello\":\"world\"}}");
    } else {
        snprintf(buf, sizeof buf, "{\"data\":null,\"errors\":[{\"message\":\"unknown field\"}]}");
    }
    return veil_kit_graphql_set_result(op, buf, strlen(buf));
}

/* ---- ソケットを開かない試験（kit --selftest） ---- */

static int selftest(void) {
    int fails = 0;
#define EXPECT(c)                                                \
    do {                                                         \
        if (!(c)) {                                              \
            fprintf(stderr, "selftest: %s failed (line %d)\n", #c, __LINE__); \
            fails++;                                             \
        }                                                        \
    } while (0)
    veil_config_t *cfg = veil_config_new();
    veil_kit_register_all(cfg, (const veil_kit_route_t[]){VEIL_KIT_GET("/api/hello", hello)}, 1, NULL);
    veil_route_options_t ro = VEIL_ROUTE_OPTIONS_INIT;
    ro.body_schema_json = USER_SCHEMA;
    veil_register_route_ex2(cfg, "/users", "POST", create_user, NULL, &ro);
    veil_register_route(cfg, "/cookie/visit", "GET", cookie_visit, NULL);
    veil_register_middleware(cfg, "/mw", NULL, mw_after, NULL);
    veil_register_middleware(cfg, "/mw", mw_before, NULL, NULL);
    veil_register_route(cfg, "/mw/echo", "GET", mw_echo, NULL);
    veil_register_route(cfg, "/deferred", "GET", deferred, NULL);

    /* コア: veil_test_* */
    veil_test_t *t = veil_test_new(cfg);
    EXPECT(t != NULL);
    if (t == NULL) return 1;
    veil_test_request_t r = VEIL_TEST_REQUEST_INIT;
    veil_test_response_t *out = NULL;
    size_t n = 0;
    r.target = "/api/hello";
    EXPECT(veil_test_request(t, &r, &out) == VEIL_OK);
    EXPECT(veil_test_response_status(out) == 200);
    const uint8_t *b = veil_test_response_body(out, &n);
    EXPECT(n == 5 && memcmp(b, "hello", 5) == 0);
    veil_test_response_free(out);
    r.target = "/nope";
    EXPECT(veil_test_request(t, &r, &out) == VEIL_OK && veil_test_response_status(out) == 404);
    veil_test_response_free(out);
    r.target = "/api/hello";
    r.method = "DELETE";
    EXPECT(veil_test_request(t, &r, &out) == VEIL_OK && veil_test_response_status(out) == 405);
    veil_test_response_free(out);
    r.method = "POST";
    r.target = "/users";
    r.body = (const uint8_t *)"{\"name\":\"\",\"x\":1}";
    r.body_len = strlen((const char *)r.body);
    EXPECT(veil_test_request(t, &r, &out) == VEIL_OK && veil_test_response_status(out) == 422);
    b = veil_test_response_body(out, &n);
    EXPECT(contains(b, n, "\"missing\"") && contains(b, n, "extra_forbidden") && contains(b, n, "too_short"));
    veil_test_response_free(out);
    r.body = (const uint8_t *)"{\"name\":\"ann\",\"age\":3}";
    r.body_len = strlen((const char *)r.body);
    EXPECT(veil_test_request(t, &r, &out) == VEIL_OK && veil_test_response_status(out) == 201);
    veil_test_response_free(out);
    static const char *const hs[] = {"x-request-id", "t-1"};
    veil_test_request_t m = VEIL_TEST_REQUEST_INIT;
    m.target = "/mw/echo";
    m.headers = hs;
    m.n_headers = 1;
    m.version = VEIL_HTTP_2;
    EXPECT(veil_test_request(t, &m, &out) == VEIL_OK);
    const char *h = veil_test_response_header(out, "x-mw", &n);
    EXPECT(h != NULL && n == 5 && memcmp(h, "after", 5) == 0);
    veil_test_response_free(out);
    veil_test_request_t d = VEIL_TEST_REQUEST_INIT;
    d.target = "/deferred";
    EXPECT(veil_test_request(t, &d, &out) == VEIL_OK);
    b = veil_test_response_body(out, &n);
    EXPECT(n == 5 && memcmp(b, "later", 5) == 0);
    veil_test_response_free(out);
    veil_test_free(t);

    /* kit: Cookie の入れ物 */
    cfg = veil_config_new();
    veil_register_route(cfg, "/cookie/visit", "GET", cookie_visit, NULL);
    veil_register_route_ex2(cfg, "/users", "POST", create_user, NULL, &ro);
    veil_kit_test_t *kt = veil_kit_test_new(cfg);
    EXPECT(kt != NULL);
    if (kt == NULL) return 1;
    for (int i = 1; i <= 3; i++) {
        EXPECT(veil_kit_test_get(kt, "/cookie/visit", &out) == VEIL_OK);
        veil_test_response_free(out);
    }
    const char *v = veil_kit_test_cookie(kt, "visits", &n);
    EXPECT(v != NULL && n == 1 && v[0] == '3');
    const char *ok_json = "{\"name\":\"bo\",\"age\":30}";
    EXPECT(veil_kit_test_post_json(kt, "/users", ok_json, strlen(ok_json), &out) == VEIL_OK);
    EXPECT(veil_test_response_status(out) == 201);
    veil_test_response_free(out);
    veil_kit_test_free(kt);
    if (fails == 0) printf("selftest ok\n");
    return fails == 0 ? 0 : 1;
#undef EXPECT
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
    VEIL_KIT_GET("/json", json_doc),
    VEIL_KIT_GET("/flaky", flaky),
    VEIL_KIT_GET("/client/sync", client_sync),
    {"/client/inline", "GET", client_inline, VEIL_ROUTE_INLINE, NULL},
    VEIL_KIT_GET("/client/json", client_json),
    VEIL_KIT_GET("/client/retry", client_retry),
    VEIL_KIT_GET("/client/async", client_async),
    VEIL_KIT_GET("/deferred", deferred),
    VEIL_KIT_GET("/grpc/unary", grpc_unary),
    VEIL_KIT_GET("/grpc/balanced", grpc_balanced),
    VEIL_KIT_GET("/di", di_route),
};

#define CHECK(x)                                                                       \
    do {                                                                               \
        if ((x) != VEIL_OK) {                                                          \
            fprintf(stderr, "%s failed: %s\n", #x, veil_last_error() ? veil_last_error() : "?"); \
            return 1;                                                                  \
        }                                                                              \
    } while (0)

int main(int argc, char **argv) {
    printf("veil kit %s\n", veil_kit_version());
    if (argc > 1 && strcmp(argv[1], "--selftest") == 0) return selftest();
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

    /* JSON 本文の検証（合わなければハンドラの前に 422） */
    veil_route_options_t ro = VEIL_ROUTE_OPTIONS_INIT;
    ro.body_schema_json = USER_SCHEMA;
    CHECK(veil_register_route_ex2(cfg, "/users", "POST", create_user, NULL, &ro));

    /* HTTP クライアント・gRPC（自分自身へ。1 台目の Balanced の先は落ちているポート） */
    snprintf(g_self, sizeof g_self, "http://%s", listen ? listen : "127.0.0.1:8080");
    veil_client_options_t co = VEIL_CLIENT_OPTIONS_INIT;
    co.timeout_ms = 5000;
    g_client = veil_client_new(&co);
    g_chan = veil_grpc_channel_new(g_self, NULL);
    const char *dead = getenv("VEIL_DEAD_URL");
    const char *urls[] = {dead ? dead : "http://127.0.0.1:1", g_self};
    g_lb = veil_kit_grpc_balanced_new(urls, 2, NULL);
    if (g_client == NULL || g_chan == NULL || g_lb == NULL) {
        fprintf(stderr, "client: %s\n", veil_last_error() ? veil_last_error() : "?");
        return 1;
    }
    CHECK(veil_register_grpc(cfg, "/kit.Echo/Echo", VEIL_GRPC_UNARY, grpc_echo, NULL));

    /* DI: 値（singleton）・要求ごと・取り出すたび */
    static long config_value = 42;
    g_di = veil_kit_di_new();
    CHECK(veil_kit_di_value(g_di, "config", &config_value, NULL));
    CHECK(veil_kit_di_register(g_di, "scoped", VEIL_KIT_DI_SCOPED, make_counter, free, NULL));
    CHECK(veil_kit_di_register(g_di, "transient", VEIL_KIT_DI_TRANSIENT, make_counter, free, NULL));

    /* GraphQL（GraphiQL も出す） */
    veil_kit_graphql_options_t go = VEIL_KIT_GRAPHQL_OPTIONS_INIT;
    go.graphiql = VEIL_ON;
    CHECK(veil_kit_graphql(cfg, "/graphql", gql_exec, NULL, &go));

    int rc = veil_run(cfg);
    veil_kit_grpc_balanced_free(g_lb);
    veil_grpc_channel_free(g_chan);
    veil_client_free(g_client);
    veil_kit_di_free(g_di);
    veil_kit_jwt_verifier_free(g_jwt);
    veil_kit_key_free(g_key);
    if (rc != VEIL_OK) {
        fprintf(stderr, "veil_run: %s\n", veil_last_error() ? veil_last_error() : "?");
        return 1;
    }
    return 0;
}
