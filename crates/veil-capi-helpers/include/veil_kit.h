/*
 * veil_kit.h - helpers kit for the veil C API (libveil_kit)
 *
 * libveil_kit contains every function of libveil (veil.h) plus the veil_kit_* functions below, built on the
 * Rust kit (veil-proxy-kit): cookies, signed and private cookies, sessions, Basic / Bearer / API key / JWT
 * authentication and guards, redirects, single files, form fields, JSON strings and app tasks. Cookies, sessions
 * and tokens use the same formats as the Rust kit, so a Rust and a C service with the same secret read each
 * other's cookies and tokens.
 *
 * Link a program with either -lveil or -lveil_kit, never both (two copies of the runtime would not work).
 *
 * Conventions are those of veil.h: functions return VEIL_OK or a negative error code (veil_last_error() tells
 * why); strings returned through `size_t *len` are not NUL-terminated and stay valid until the callback returns;
 * strings returned as `char *` are NUL-terminated and freed with veil_kit_free(); structures start with
 * `size_t size` and a zero-filled structure means the defaults.
 */
#ifndef VEIL_KIT_H
#define VEIL_KIT_H

#include "veil.h"

#ifdef __cplusplus
extern "C" {
#endif

/* A token is missing, malformed, expired or signed with an unknown key (veil_kit_jwt_verify). */
#define VEIL_KIT_ERR_TOKEN (-20)

/* The kit's version (static; the same as veil_version). Calling it confirms the program links libveil_kit. */
const char *veil_kit_version(void);
/* Frees a `char *` returned by a veil_kit_* function (NULL does nothing). */
void veil_kit_free(char *p);

/* ---- Cookies ---- */

#define VEIL_KIT_SAMESITE_DEFAULT 0 /* no SameSite attribute */
#define VEIL_KIT_SAMESITE_STRICT  1
#define VEIL_KIT_SAMESITE_LAX     2
#define VEIL_KIT_SAMESITE_NONE    3 /* also sets Secure */

/* A cookie to send. Zero-filled means defaults; size is sizeof(veil_kit_cookie_t) or 0. */
typedef struct {
    size_t size;
    const char *name;
    const char *value;
    const char *path;     /* NULL = "/", "" = no Path */
    const char *domain;   /* NULL = none */
    int64_t max_age_secs; /* 0 = none (until the browser closes), > 0 = seconds, < 0 = delete (Max-Age=0) */
    int secure;
    int http_only;
    int same_site;        /* VEIL_KIT_SAMESITE_* */
    int partitioned;      /* CHIPS (also sets Secure) */
} veil_kit_cookie_t;
#define VEIL_KIT_COOKIE_INIT { sizeof(veil_kit_cookie_t) }

/* The first cookie named `name` in the request (every Cookie header), or NULL. */
const char *veil_kit_cookie_get(const veil_request_t *req, const char *name, size_t *len);
/* Adds Set-Cookie (call it again for more cookies). Invalid names, values and attributes (RFC 6265bis, the
 * __Secure- and __Host- prefixes) are not sent: VEIL_ERR_INVALID_ARG. */
int veil_kit_set_cookie(veil_response_t *resp, const veil_kit_cookie_t *cookie);
/* Deletes the cookie in the browser (Max-Age=0, Path=/). */
int veil_kit_remove_cookie(veil_response_t *resp, const char *name);

/* The secret of signed and private cookies and sessions. Keys are safe to use from several threads. */
typedef struct veil_kit_key veil_kit_key_t;
/* From 64 bytes of secret material (NULL when len is not 64). */
veil_kit_key_t *veil_kit_key_from(const uint8_t *material, size_t len);
/* Derived from a secret of at least 32 bytes, exactly like the Rust kit's Key::derive_from (NULL if shorter). */
veil_kit_key_t *veil_kit_key_derive(const uint8_t *secret, size_t len);
/* A random key (cookies do not survive a restart). */
veil_kit_key_t *veil_kit_key_generate(void);
/* Also accepts cookies made with `old` (key rotation). Call it before requests use the key. */
int veil_kit_key_add_fallback(veil_kit_key_t *key, const veil_kit_key_t *old);
void veil_kit_key_free(veil_kit_key_t *key);

/* Signed cookies (readable, not changeable by the browser): the verified value, or NULL. */
const char *veil_kit_signed_get(const veil_request_t *req, const veil_kit_key_t *key, const char *name, size_t *len);
/* Sends cookie->value signed with `key` (the other fields as in veil_kit_set_cookie). */
int veil_kit_signed_set(veil_response_t *resp, const veil_kit_key_t *key, const veil_kit_cookie_t *cookie);
/* Private cookies (encrypted and authenticated, AES-256-GCM): the decrypted value, or NULL. */
const char *veil_kit_private_get(const veil_request_t *req, const veil_kit_key_t *key, const char *name, size_t *len);
int veil_kit_private_set(veil_response_t *resp, const veil_kit_key_t *key, const veil_kit_cookie_t *cookie);

/* ---- Sessions ---- */

/* Zero-filled means defaults; size is sizeof(veil_kit_sessions_t) or 0. */
typedef struct {
    size_t size;
    const char *prefix;      /* only routes under this prefix (NULL = "/") */
    const char *cookie_name; /* NULL = "veil_session" */
    uint64_t ttl_secs;       /* 0 = 24 hours (renewed on each change) */
    int secure;              /* VEIL_DEFAULT (on) / VEIL_OFF (plain-HTTP development only) */
    int same_site;           /* VEIL_KIT_SAMESITE_* (0 = Lax) */
    int memory_store;        /* 0 = the data in one encrypted cookie; non-zero = in server memory, a signed id in the cookie */
    size_t max_entries;      /* memory store limit (0 = 100000) */
} veil_kit_sessions_t;
#define VEIL_KIT_SESSIONS_INIT { sizeof(veil_kit_sessions_t) }
/* Installs sessions (the Rust kit's Sessions; `opts` may be NULL). The configuration keeps its own reference to
 * `key`. The session is loaded before the C middleware and handler and saved after them; changes made after
 * veil_response_stream are not saved. */
int veil_kit_sessions_install(veil_config_t *cfg, const veil_kit_key_t *key, const veil_kit_sessions_t *opts);
/* Values are JSON. _get returns the JSON text, _get_str the unquoted text of a JSON string (NULL otherwise).
 * VEIL_ERR_STATE when the route has no session. */
const char *veil_kit_session_get(const veil_request_t *req, const char *key, size_t *len);
const char *veil_kit_session_get_str(const veil_request_t *req, const char *key, size_t *len);
int veil_kit_session_set(const veil_request_t *req, const char *key, const char *json);
int veil_kit_session_set_str(const veil_request_t *req, const char *key, const char *value);
int veil_kit_session_remove(const veil_request_t *req, const char *key);
int veil_kit_session_clear(const veil_request_t *req);
/* Ends the session (deletes the cookie, and the stored data with the memory store). */
int veil_kit_session_destroy(const veil_request_t *req);
/* A new session id with the same data: call it after a login (session fixation). */
int veil_kit_session_cycle_id(const veil_request_t *req);

/* ---- Authentication ---- */

/* Authorization: Basic. 1 with the user and password, 0 when absent or malformed. Output pointers may be NULL. */
int veil_kit_basic_auth(const veil_request_t *req, const char **user, size_t *user_len, const char **password,
                        size_t *password_len);
/* 1 when the Basic credentials are user / password (constant-time comparison). */
int veil_kit_basic_matches(const veil_request_t *req, const char *user, const char *password);
/* Sets resp to 401 with WWW-Authenticate: <challenge> (NULL = "Bearer"; for example "Basic realm=\"admin\""). */
int veil_kit_unauthorized(veil_response_t *resp, const char *challenge);
/* The token of Authorization: Bearer, or NULL. */
const char *veil_kit_bearer(const veil_request_t *req, size_t *len);
/* The API key header (NULL = X-API-Key), or NULL when absent or empty. */
const char *veil_kit_api_key(const veil_request_t *req, const char *header, size_t *len);
/* 1 when the API key header is one of `keys` (constant-time comparison of each). */
int veil_kit_api_key_matches(const veil_request_t *req, const char *header, const char *const *keys, size_t n);

/* The authenticated subject of a request (the Rust kit's Principal; Rust plugins and C code see the same one). */
typedef struct veil_kit_principal veil_kit_principal_t;
veil_kit_principal_t *veil_kit_principal_new(const char *subject);
int veil_kit_principal_add_scope(veil_kit_principal_t *p, const char *scope);
int veil_kit_principal_add_role(veil_kit_principal_t *p, const char *role);
/* Extra attributes as a JSON value (for example the claims of a token). */
int veil_kit_principal_set_attributes(veil_kit_principal_t *p, const char *json);
/* Frees a principal that was not given to veil_kit_set_principal. */
void veil_kit_principal_free(veil_kit_principal_t *p);
/* Makes `p` the request's principal and takes ownership of it (call it from a middleware's before function). */
int veil_kit_set_principal(const veil_request_t *req, veil_kit_principal_t *p);
/* The request's principal, or NULL (valid until the callback returns or the principal is replaced). */
const veil_kit_principal_t *veil_kit_principal(const veil_request_t *req);
const char *veil_kit_principal_subject(const veil_kit_principal_t *p, size_t *len);
int veil_kit_principal_has_scope(const veil_kit_principal_t *p, const char *scope);
int veil_kit_principal_has_role(const veil_kit_principal_t *p, const char *role);
/* The attributes as JSON ("null" when none); free with veil_kit_free. */
char *veil_kit_principal_attributes(const veil_kit_principal_t *p);

/* A guard (C middleware) for the C routes under `prefix` (NULL = "/"): 401 without a principal, 403 when the
 * principal lacks `scope` (NULL = any principal). Register it BEFORE the middleware that sets the principal:
 * the middleware registered last runs first. */
int veil_kit_require_scope(veil_config_t *cfg, const char *prefix, const char *scope);

/* ---- JWT ---- */

/* Verifies tokens like the Rust kit: the algorithm is fixed by each key ("none" is never accepted), keys are
 * chosen by kid, exp is required by default, 60 s of clock leeway. Configure it before it is used; afterwards
 * only veil_kit_jwt_verifier_set_jwks changes it (VEIL_ERR_STATE otherwise). Safe to use from several threads. */
typedef struct veil_kit_jwt_verifier veil_kit_jwt_verifier_t;
veil_kit_jwt_verifier_t *veil_kit_jwt_verifier_new(void);
void veil_kit_jwt_verifier_free(veil_kit_jwt_verifier_t *v);
/* alg: "HS256", "HS384" or "HS512"; kid may be NULL. */
int veil_kit_jwt_verifier_add_hmac(veil_kit_jwt_verifier_t *v, const char *alg, const uint8_t *secret, size_t len,
                                   const char *kid);
/* alg: "RS256".."RS512", "PS256".."PS512", "ES256", "ES384" or "EdDSA"; a public key in PEM. */
int veil_kit_jwt_verifier_add_pem(veil_kit_jwt_verifier_t *v, const char *alg, const char *pem, const char *kid);
/* The keys of a JWK Set ({"keys": [...]}; unusable keys are skipped). */
int veil_kit_jwt_verifier_add_jwks(veil_kit_jwt_verifier_t *v, const char *jwks_json);
/* Replaces the keys while the server runs (from any thread), for example after fetching the provider's JWKS. */
int veil_kit_jwt_verifier_set_jwks(veil_kit_jwt_verifier_t *v, const char *jwks_json);
int veil_kit_jwt_verifier_issuer(veil_kit_jwt_verifier_t *v, const char *issuer);
int veil_kit_jwt_verifier_audience(veil_kit_jwt_verifier_t *v, const char *audience);
int veil_kit_jwt_verifier_require_exp(veil_kit_jwt_verifier_t *v, int on);
int veil_kit_jwt_verifier_leeway(veil_kit_jwt_verifier_t *v, uint64_t secs);
/* VEIL_OK with the claims as JSON in *claims_json (NULL to skip; free with veil_kit_free), or
 * VEIL_KIT_ERR_TOKEN with the reason in veil_last_error. */
int veil_kit_jwt_verify(veil_kit_jwt_verifier_t *v, const char *token, size_t len, char **claims_json);
/* Signs a JSON object of claims with an HMAC key; *out is the token (free with veil_kit_free). */
int veil_kit_jwt_encode_hmac(const char *alg, const uint8_t *secret, size_t len, const char *kid,
                             const char *claims_json, char **out);

/* Requests without a token continue anonymously (veil_kit_jwt_authenticate). */
#define VEIL_KIT_JWT_OPTIONAL 1u
/* A C middleware for the routes under `prefix` that verifies Authorization: Bearer and makes the claims the
 * request's principal (sub, the scopes of "scope", "roles"; the claims are its attributes). A missing or invalid
 * token gets 401 (WWW-Authenticate: Bearer error="invalid_token" when invalid). Register veil_kit_require_scope
 * before it. The configuration keeps its own reference to `v`. */
int veil_kit_jwt_authenticate(veil_config_t *cfg, const char *prefix, veil_kit_jwt_verifier_t *v, uint32_t flags);

/* ---- Redirects, files, forms, errors, JSON ---- */

/* status: 301, 302, 303, 307 or 308 (0 = 302). Control characters in the location are dropped. */
int veil_kit_redirect(veil_response_t *resp, uint16_t status, const char *location);
/* Redirects only to a path on this site (not //host) or to one of `allowed_hosts`; otherwise resp is unchanged
 * and the result is VEIL_ERR_INVALID_ARG (redirect to a fixed page instead). Against open redirects. */
int veil_kit_redirect_safe(veil_response_t *resp, uint16_t status, const char *target,
                           const char *const *allowed_hosts, size_t n);
/* Sets resp to the file with a Content-Type from its extension (404, 403, or 413 above max_size). Not on
 * VEIL_ROUTE_INLINE routes (VEIL_ERR_STATE). Do not pass paths from the request unchecked. */
int veil_kit_send_file(veil_response_t *resp, const char *path, uint64_t max_size);
/* The first value of `name` in an application/x-www-form-urlencoded body (decoded), or NULL. */
const char *veil_kit_form_get(const veil_request_t *req, const char *name, size_t *len);
/* Sets resp to {"detail": "<detail>"} with `status`. */
int veil_kit_error(veil_response_t *resp, uint16_t status, const char *detail);
/* `s` as a quoted and escaped JSON string; free with veil_kit_free. */
char *veil_kit_json_string(const char *s);
/* The OpenAPI security scheme of the OAuth2 password flow (for components.securitySchemes); free with
 * veil_kit_free. */
char *veil_kit_oauth2_security_scheme(const char *token_url);

/* ---- App tasks ---- */

/* Task functions run on the handler pool. A schedule runs on one worker; runs never overlap, and no new run
 * starts after graceful shutdown begins. For work after a response, use veil_response_after_send. */
typedef void (*veil_kit_task_fn)(void *user_data);
/* Every period_ms milliseconds (the first run one period after startup). */
int veil_kit_every(veil_config_t *cfg, uint64_t period_ms, veil_kit_task_fn func, void *user_data);
/* On a cron schedule in UTC: "sec min hour day month weekday" or the usual five fields. */
int veil_kit_cron(veil_config_t *cfg, const char *expr, veil_kit_task_fn func, void *user_data);

/* ---- Route tables (header-only) ---- */

typedef struct {
    const char *path;
    const char *methods;      /* "GET", "GET,POST", or NULL for every method */
    veil_handler_fn handler;
    uint32_t flags;           /* VEIL_ROUTE_* */
    const char *openapi_json; /* OpenAPI Operation Object (NULL = none) */
} veil_kit_route_t;
#define VEIL_KIT_ROUTE(m, p, h) { (p), (m), (h), 0u, NULL }
#define VEIL_KIT_GET(p, h)      VEIL_KIT_ROUTE("GET", p, h)
#define VEIL_KIT_POST(p, h)     VEIL_KIT_ROUTE("POST", p, h)
#define VEIL_KIT_PUT(p, h)      VEIL_KIT_ROUTE("PUT", p, h)
#define VEIL_KIT_DELETE(p, h)   VEIL_KIT_ROUTE("DELETE", p, h)
#define VEIL_KIT_PATCH(p, h)    VEIL_KIT_ROUTE("PATCH", p, h)
/* Registers `n` routes with the same user_data; stops at the first error (its code is returned). */
static inline int veil_kit_register_all(veil_config_t *cfg, const veil_kit_route_t *routes, size_t n, void *user_data) {
    size_t i;
    for (i = 0; i < n; i++) {
        int rc = veil_register_route_ex(cfg, routes[i].path, routes[i].methods, routes[i].handler, user_data,
                                        routes[i].flags, routes[i].openapi_json);
        if (rc != VEIL_OK) return rc;
    }
    return VEIL_OK;
}

#ifdef __cplusplus
}
#endif

#endif /* VEIL_KIT_H */
