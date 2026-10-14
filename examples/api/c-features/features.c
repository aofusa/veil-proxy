/*
 * veil C API の F-210 の機能のサンプル: 応答のストリーミング (SSE)・要求本文のストリーミング受信・
 * gRPC (examples/api/rust-grpc と同じ demo.v1.Demo)・L4 (TCP・UDP のエコー)。
 *
 * gRPC のメッセージは小さな protobuf を手で符号化している (veil は protobuf の実装に依存しない。
 * 実際のアプリでは protobuf-c・nanopb・upb 等を使う)。
 *
 *   VEIL_LISTEN=127.0.0.1:8080 VEIL_L4_TCP=127.0.0.1:7000 VEIL_L4_UDP=127.0.0.1:7001 ./features
 *   VEIL_TLS_LISTEN=127.0.0.1:8443 VEIL_TLS_CERT=cert.pem VEIL_TLS_KEY=key.pem で TLS + HTTP/3 も
 */
#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
static void sleep_ms(unsigned ms) { Sleep(ms); }
#else
#include <time.h>
static void sleep_ms(unsigned ms) {
    struct timespec ts = {(time_t)(ms / 1000), (long)(ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}
#endif

#include "veil.h"

/* ===================== HTTP ===================== */

/* GET /sse : Server-Sent Events を 3 件、別スレッドを使わずにコールバックの中で書く */
static int sse(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)req;
    (void)ud;
    veil_response_set_header(resp, "content-type", "text/event-stream");
    veil_stream_t *s = veil_response_stream(resp);
    if (s == NULL) {
        return 1;
    }
    for (int i = 1; i <= 3; i++) {
        char ev[64];
        int n = snprintf(ev, sizeof ev, "data: tick %d\n\n", i);
        if (veil_stream_write(s, (const uint8_t *)ev, (size_t)n) != VEIL_OK) {
            break; /* クライアントが切断した */
        }
        sleep_ms(10);
    }
    veil_stream_set_trailer(s, "x-ticks", "3");
    veil_stream_close(s);
    return 0;
}

/* POST /count : 要求本文を溜めずに順に読み、バイト数を返す (VEIL_ROUTE_STREAM_BODY) */
static int count(const veil_request_t *req, veil_response_t *resp, void *ud) {
    (void)ud;
    uint8_t buf[4096];
    long long total = 0;
    int64_t n;
    while ((n = veil_request_body_read(req, buf, sizeof buf)) > 0) {
        total += n;
    }
    if (n < 0) {
        return 1;
    }
    char out[32];
    snprintf(out, sizeof out, "%lld", total);
    return veil_response_set_text(resp, out);
}

/* ===================== protobuf (最小) ===================== */

typedef struct {
    uint8_t *p;
    size_t len, cap;
} buf_t;

static void put(buf_t *b, const void *d, size_t n) {
    if (b->len + n > b->cap) {
        b->cap = (b->len + n) * 2 + 16;
        b->p = realloc(b->p, b->cap);
    }
    memcpy(b->p + b->len, d, n);
    b->len += n;
}

static void put_varint(buf_t *b, uint64_t v) {
    uint8_t t[10];
    size_t n = 0;
    while (v >= 0x80) {
        t[n++] = (uint8_t)(v | 0x80);
        v >>= 7;
    }
    t[n++] = (uint8_t)v;
    put(b, t, n);
}

static void put_string(buf_t *b, int field, const char *s, size_t len) {
    if (len == 0) {
        return;
    }
    put_varint(b, ((uint64_t)field << 3) | 2);
    put_varint(b, len);
    put(b, s, len);
}

static void put_uint(buf_t *b, int field, uint64_t v) {
    if (v == 0) {
        return;
    }
    put_varint(b, ((uint64_t)field << 3) | 0);
    put_varint(b, v);
}

/* 1 フィールドだけ読む (他は読み飛ばす)。見つかれば 1。 */
static int get_varint(const uint8_t *p, size_t len, size_t *pos, uint64_t *v) {
    *v = 0;
    for (int i = 0; i < 10 && *pos < len; i++) {
        uint8_t c = p[(*pos)++];
        *v |= (uint64_t)(c & 0x7f) << (7 * i);
        if (c < 0x80) {
            return 1;
        }
    }
    return 0;
}

static int find_field(const uint8_t *p, size_t len, int field, uint64_t *v, const uint8_t **s, size_t *slen) {
    size_t pos = 0;
    while (pos < len) {
        uint64_t key;
        if (!get_varint(p, len, &pos, &key)) {
            return -1;
        }
        int f = (int)(key >> 3), w = (int)(key & 7);
        if (w == 0) {
            uint64_t x;
            if (!get_varint(p, len, &pos, &x)) {
                return -1;
            }
            if (f == field) {
                *v = x;
                return 1;
            }
        } else if (w == 2) {
            uint64_t n;
            if (!get_varint(p, len, &pos, &n) || pos + n > len) {
                return -1;
            }
            if (f == field) {
                *s = p + pos;
                *slen = (size_t)n;
                return 1;
            }
            pos += (size_t)n;
        } else if (w == 1) {
            pos += 8;
        } else if (w == 5) {
            pos += 4;
        } else {
            return -1;
        }
    }
    return 0;
}

static int send_buf(veil_grpc_call_t *call, buf_t *b) {
    int rc = veil_grpc_send(call, b->p, b->len);
    free(b->p);
    b->p = NULL;
    b->len = b->cap = 0;
    return rc;
}

/* ===================== gRPC: demo.v1.Demo ===================== */

static int say_hello(veil_grpc_call_t *call, void *ud) {
    (void)ud;
    const uint8_t *m;
    size_t n;
    if (veil_grpc_recv(call, &m, &n) != 1) {
        return 1;
    }
    const uint8_t *name = (const uint8_t *)"";
    size_t nlen = 0;
    uint64_t dummy;
    find_field(m, n, 1, &dummy, &name, &nlen);
    size_t clen = 0;
    const char *caller = veil_grpc_metadata(call, "x-caller", &clen);
    if (caller != NULL && clen < 256) {
        char c[256];
        memcpy(c, caller, clen);
        c[clen] = '\0';
        veil_grpc_set_header(call, "x-caller-echo", c);
    }
    buf_t reply = {0};
    char *msg = malloc(nlen + 6);
    memcpy(msg, "Hello ", 6);
    memcpy(msg + 6, name, nlen);
    put_string(&reply, 1, msg, nlen + 6);
    free(msg);
    veil_grpc_set_trailer(call, "x-served-by", "veil-c");
    return send_buf(call, &reply) == VEIL_OK ? 0 : 1;
}

static int count_up(veil_grpc_call_t *call, void *ud) {
    (void)ud;
    const uint8_t *m, *s;
    size_t n, sl;
    uint64_t count = 0;
    if (veil_grpc_recv(call, &m, &n) != 1) {
        return 1;
    }
    find_field(m, n, 1, &count, &s, &sl);
    if (count > 100000) {
        return veil_grpc_set_status(call, 3, "count must be <= 100000"); /* INVALID_ARGUMENT */
    }
    for (uint64_t i = 1; i <= count; i++) {
        buf_t r = {0};
        put_uint(&r, 1, i);
        if (send_buf(call, &r) != VEIL_OK) {
            return 0; /* クライアントが取り消した */
        }
    }
    return 0;
}

static int sum(veil_grpc_call_t *call, void *ud) {
    (void)ud;
    const uint8_t *m, *s;
    size_t n, sl;
    int64_t total = 0;
    uint64_t cnt = 0;
    int rc;
    while ((rc = veil_grpc_recv(call, &m, &n)) == 1) {
        uint64_t v = 0;
        find_field(m, n, 1, &v, &s, &sl);
        total += (int64_t)v;
        cnt++;
    }
    if (rc < 0) {
        return 0; /* 状態は veil が設定済み */
    }
    buf_t r = {0};
    put_uint(&r, 1, (uint64_t)total);
    put_uint(&r, 2, cnt);
    return send_buf(call, &r) == VEIL_OK ? 0 : 1;
}

static int chat(veil_grpc_call_t *call, void *ud) {
    (void)ud;
    const uint8_t *m;
    size_t n;
    while (veil_grpc_recv(call, &m, &n) == 1) {
        const uint8_t *t = (const uint8_t *)"";
        size_t tl = 0;
        uint64_t dummy;
        find_field(m, n, 1, &dummy, &t, &tl);
        char *up = malloc(tl + 1);
        for (size_t i = 0; i < tl; i++) {
            up[i] = (char)toupper(t[i]);
        }
        buf_t r = {0};
        put_string(&r, 1, up, tl);
        free(up);
        if (send_buf(call, &r) != VEIL_OK) {
            break;
        }
    }
    return 0;
}

static int sleep_rpc(veil_grpc_call_t *call, void *ud) {
    (void)ud;
    const uint8_t *m, *s;
    size_t n, sl;
    uint64_t ms = 0;
    if (veil_grpc_recv(call, &m, &n) == 1) {
        find_field(m, n, 1, &ms, &s, &sl);
    }
    if (ms > 60000) {
        ms = 60000;
    }
    /* 期限を見ながら少しずつ待つ (期限を過ぎたら veil がクライアントへ DEADLINE_EXCEEDED を返している) */
    for (uint64_t waited = 0; waited < ms; waited += 10) {
        int64_t left = veil_grpc_deadline_ms(call);
        if (left == 0) {
            return veil_grpc_set_status(call, 4, "deadline exceeded");
        }
        sleep_ms(10);
    }
    return veil_grpc_send(call, NULL, 0) == VEIL_OK ? 0 : 1; /* Empty */
}

static int fail_rpc(veil_grpc_call_t *call, void *ud) {
    (void)ud;
    const uint8_t *m, *s = (const uint8_t *)"";
    size_t n, sl = 0;
    uint64_t code = 2, dummy;
    if (veil_grpc_recv(call, &m, &n) == 1) {
        find_field(m, n, 1, &code, &s, &sl);
        const uint8_t *s2 = NULL;
        if (find_field(m, n, 2, &dummy, &s2, &sl) == 1) {
            s = s2;
        } else {
            sl = 0;
        }
    }
    char msg[256];
    snprintf(msg, sizeof msg, "%.*s", (int)(sl > 200 ? 200 : sl), (const char *)s);
    return veil_grpc_set_status(call, (int)code, msg);
}

/* ===================== L4 ===================== */

static int tcp_data(veil_l4_conn_t *conn, const uint8_t *data, size_t len, void *ud) {
    (void)ud;
    return veil_l4_write(conn, data, len) == VEIL_OK ? 0 : 1;
}

static int udp_echo(const uint8_t *data, size_t len, const char *peer, veil_l4_reply_t *reply, void *ud) {
    (void)peer;
    (void)ud;
    uint8_t out[2048];
    if (len + 5 > sizeof out) {
        return 1;
    }
    memcpy(out, "echo:", 5);
    memcpy(out + 5, data, len);
    return veil_l4_reply(reply, out, len + 5);
}

static int check(int rc, const char *what) {
    if (rc != VEIL_OK) {
        fprintf(stderr, "%s: %s\n", what, veil_last_error());
        exit(1);
    }
    return rc;
}

int main(void) {
    veil_config_t *cfg = veil_config_new();
    const char *listen = getenv("VEIL_LISTEN");
    check(veil_config_listen_cleartext(cfg, listen ? listen : "127.0.0.1:8080"), "listen");
    const char *tls = getenv("VEIL_TLS_LISTEN"), *cert = getenv("VEIL_TLS_CERT"), *key = getenv("VEIL_TLS_KEY");
    if (tls && cert && key) {
        char lit[300];
        check(veil_config_listen(cfg, tls), "tls listen");
        check(veil_config_tls(cfg, cert, key), "tls");
        check(veil_config_set(cfg, "http3.enabled", "true"), "http3");
        snprintf(lit, sizeof lit, "\"%s\"", tls);
        check(veil_config_set(cfg, "http3.listen", lit), "http3 listen");
    }
    check(veil_config_set(cfg, "logging.level", "\"warn\""), "log");
    /* 1KB 以上の gRPC の応答を gzip で（クライアントが受けるときだけ。F-209） */
    check(veil_config_set(cfg, "api.grpc_response_compression", "\"gzip\""), "compression");

    check(veil_register_route(cfg, "/sse", "GET", sse, NULL), "sse");
    check(veil_register_route_ex(cfg, "/count", "POST", count, NULL, VEIL_ROUTE_STREAM_BODY, NULL), "count");

    check(veil_register_grpc(cfg, "/demo.v1.Demo/SayHello", VEIL_GRPC_UNARY, say_hello, NULL), "SayHello");
    check(veil_register_grpc(cfg, "/demo.v1.Demo/CountUp", VEIL_GRPC_SERVER_STREAMING, count_up, NULL), "CountUp");
    check(veil_register_grpc(cfg, "/demo.v1.Demo/Sum", VEIL_GRPC_CLIENT_STREAMING, sum, NULL), "Sum");
    check(veil_register_grpc(cfg, "/demo.v1.Demo/Chat", VEIL_GRPC_BIDI, chat, NULL), "Chat");
    check(veil_register_grpc(cfg, "/demo.v1.Demo/Sleep", VEIL_GRPC_UNARY, sleep_rpc, NULL), "Sleep");
    check(veil_register_grpc(cfg, "/demo.v1.Demo/Fail", VEIL_GRPC_UNARY, fail_rpc, NULL), "Fail");
    check(veil_register_grpc_health(cfg), "health");

    const char *l4t = getenv("VEIL_L4_TCP"), *l4u = getenv("VEIL_L4_UDP");
    if (l4t) {
        veil_l4_tcp_callbacks_t cb = {NULL, tcp_data, NULL};
        check(veil_register_l4_tcp(cfg, "c_tcp_echo", l4t, &cb, NULL), "l4 tcp");
    }
    if (l4u) {
        check(veil_register_l4_udp(cfg, "c_udp_echo", l4u, udp_echo, NULL), "l4 udp");
    }
    if (veil_run(cfg) != VEIL_OK) {
        fprintf(stderr, "veil_run: %s\n", veil_last_error());
        return 1;
    }
    return 0;
}
