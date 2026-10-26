// F-267: Cookie のセッション。中身を暗号化した Cookie に入れるか（既定）、署名付き Cookie の ID と保存先の組。
//! Sessions.
//!
//! ```no_run
//! use veil::prelude::*;
//! use veil_kit::cookie::Key;
//! use veil_kit::session::{Session, Sessions};
//!
//! #[veil::route(path = "/count", method = "GET")]
//! async fn count(session: Session) -> String {
//!     let n: u64 = session.get("n").unwrap_or(0) + 1;
//!     session.insert("n", n);
//!     n.to_string()
//! }
//!
//! App::new().with(Sessions::new(Key::generate())).run().unwrap();
//! ```
//!
//! By default the whole session is kept in one encrypted, authenticated cookie (no server state; keep it small).
//! [`Sessions::store`] keeps the data on the server ([`MemoryStore`], or your own [`SessionStore`]) and only a
//! signed session id in the cookie. Call [`Session::cycle_id`] after logging in (session fixation).

use crate::cookie::{Cookie, CookieJar, Key, ResponseCookieExt, SameSite};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use veil::api::{App, FromRequest, HandlerFn, Plugin, Request, Response};

/// Session data.
pub type Data = serde_json::Map<String, serde_json::Value>;

/// Where server-side sessions are kept. Calls run on the worker thread, so they must be quick (an in-memory map,
/// or a client with its own background I/O).
pub trait SessionStore: Send + Sync + 'static {
    /// The data of session `id`, if it exists and has not expired.
    fn load(&self, id: &str) -> Option<Data>;
    /// Saves session `id` for `ttl`.
    fn save(&self, id: &str, data: &Data, ttl: Duration);
    /// Deletes session `id`.
    fn remove(&self, id: &str);
}

/// Sessions in process memory (lost on restart, not shared between processes). Expired entries are dropped
/// lazily when the store grows.
#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<HashMap<String, (Data, Instant)>>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> MemoryStore {
        MemoryStore::default()
    }
}

impl SessionStore for MemoryStore {
    fn load(&self, id: &str) -> Option<Data> {
        let g = self.map.lock().unwrap();
        let (d, until) = g.get(id)?;
        (Instant::now() < *until).then(|| d.clone())
    }

    fn save(&self, id: &str, data: &Data, ttl: Duration) {
        let mut g = self.map.lock().unwrap();
        if g.len() >= 1024 && g.len().is_power_of_two() {
            let now = Instant::now();
            g.retain(|_, (_, until)| *until > now);
        }
        g.insert(id.to_string(), (data.clone(), Instant::now() + ttl));
    }

    fn remove(&self, id: &str) {
        self.map.lock().unwrap().remove(id);
    }
}

/// Session settings (a [`Plugin`]).
pub struct Sessions {
    key: Arc<Key>,
    cookie_name: String,
    ttl: Duration,
    secure: bool,
    same_site: SameSite,
    store: Option<Arc<dyn SessionStore>>,
    prefix: String,
}

impl Sessions {
    /// Cookie sessions named `veil_session`: `HttpOnly`, `Secure`, `SameSite=Lax`, 24 hours, for all routes.
    pub fn new(key: Key) -> Sessions {
        Sessions {
            key: Arc::new(key),
            cookie_name: "veil_session".into(),
            ttl: Duration::from_secs(24 * 3600),
            secure: true,
            same_site: SameSite::Lax,
            store: None,
            prefix: "/".into(),
        }
    }

    /// The cookie name.
    pub fn cookie_name(mut self, name: &str) -> Sessions {
        self.cookie_name = name.to_string();
        self
    }

    /// How long a session lives (renewed on each change).
    pub fn ttl(mut self, ttl: Duration) -> Sessions {
        self.ttl = ttl;
        self
    }

    /// `Secure` (default `true`; turn it off only for plain-HTTP development).
    pub fn secure(mut self, on: bool) -> Sessions {
        self.secure = on;
        self
    }

    /// `SameSite` (default `Lax`).
    pub fn same_site(mut self, s: SameSite) -> Sessions {
        self.same_site = s;
        self
    }

    /// Keeps the data in `store`; the cookie carries only a signed id.
    pub fn store(mut self, store: impl SessionStore) -> Sessions {
        self.store = Some(Arc::new(store));
        self
    }

    /// Only for routes under `prefix`.
    pub fn on(mut self, prefix: &str) -> Sessions {
        self.prefix = prefix.to_string();
        self
    }
}

struct State {
    data: Mutex<Data>,
    id: Mutex<Option<String>>,
    changed: AtomicBool,
    destroyed: AtomicBool,
    new_id: AtomicBool,
}

/// The session of the request (an extractor; needs [`Sessions`] installed).
#[derive(Clone)]
pub struct Session(Arc<State>);

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("keys", &self.0.data.lock().unwrap().len())
            .finish()
    }
}

impl Session {
    /// A value (`None` when absent or of another type).
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let g = self.0.data.lock().unwrap();
        serde_json::from_value(g.get(key)?.clone()).ok()
    }

    /// Sets a value.
    pub fn insert<T: Serialize>(&self, key: &str, value: T) {
        if let Ok(v) = serde_json::to_value(value) {
            self.0.data.lock().unwrap().insert(key.to_string(), v);
            self.0.changed.store(true, Ordering::Relaxed);
        }
    }

    /// Removes a value.
    pub fn remove(&self, key: &str) {
        if self.0.data.lock().unwrap().remove(key).is_some() {
            self.0.changed.store(true, Ordering::Relaxed);
        }
    }

    /// Removes every value (the session stays).
    pub fn clear(&self) {
        self.0.data.lock().unwrap().clear();
        self.0.changed.store(true, Ordering::Relaxed);
    }

    /// Ends the session (the cookie is deleted).
    pub fn destroy(&self) {
        self.0.destroyed.store(true, Ordering::Relaxed);
    }

    /// Gives the session a new id (keeps the data). Call it when the user logs in.
    pub fn cycle_id(&self) {
        self.0.new_id.store(true, Ordering::Relaxed);
        self.0.changed.store(true, Ordering::Relaxed);
    }

    /// Whether the session has no values.
    pub fn is_empty(&self) -> bool {
        self.0.data.lock().unwrap().is_empty()
    }
}

impl FromRequest for Session {
    fn from_request(req: &Request) -> Result<Self, Response> {
        req.extensions().get::<State>().map(Session).ok_or_else(|| {
            ftlog::error!(
                "the Session extractor needs veil_kit::session::Sessions installed with App::with"
            );
            Response::error(500, "Internal Server Error")
        })
    }
}

fn new_id() -> String {
    let mut b = [0u8; 24];
    crate::crypto::random(&mut b);
    crate::b64::encode(&b)
}

struct Shared {
    key: Arc<Key>,
    cookie_name: String,
    ttl: Duration,
    secure: bool,
    same_site: SameSite,
    store: Option<Arc<dyn SessionStore>>,
}

impl Shared {
    fn load(&self, req: &Request) -> (Data, Option<String>) {
        let jar = CookieJar::from_request_headers(req);
        let Some(raw) = jar.get_bytes(&self.cookie_name) else {
            return (Data::new(), None);
        };
        match &self.store {
            None => {
                let data = self
                    .key
                    .decrypt_value(&self.cookie_name, raw)
                    .and_then(|b| serde_json::from_slice::<Data>(&b).ok())
                    .unwrap_or_default();
                (data, None)
            }
            Some(store) => match self.key.verify_value(&self.cookie_name, raw) {
                Some(id) => match store.load(&id) {
                    Some(d) => (d, Some(id)),
                    None => (Data::new(), None),
                },
                None => (Data::new(), None),
            },
        }
    }

    fn cookie(&self, value: String) -> Cookie {
        Cookie::new(self.cookie_name.clone(), value)
            .http_only(true)
            .secure(self.secure)
            .same_site(self.same_site)
            .max_age(self.ttl)
    }

    fn finish(&self, resp: Response, st: &State) -> Response {
        let old_id = st.id.lock().unwrap().clone();
        if st.destroyed.load(Ordering::Relaxed) {
            if let (Some(store), Some(id)) = (&self.store, &old_id) {
                store.remove(id);
            }
            return resp.with_cookie(
                Cookie::removal(self.cookie_name.clone())
                    .secure(self.secure)
                    .http_only(true),
            );
        }
        if !st.changed.load(Ordering::Relaxed) {
            return resp;
        }
        let data = st.data.lock().unwrap();
        match &self.store {
            None => {
                let json = serde_json::to_vec(&*data).unwrap_or_default();
                let v = self.key.encrypt_value(&self.cookie_name, &json);
                if v.len() > 4000 {
                    ftlog::error!("session larger than a cookie can hold; use Sessions::store");
                    return resp;
                }
                resp.with_cookie(self.cookie(v))
            }
            Some(store) => {
                let id = match &old_id {
                    Some(id) if !st.new_id.load(Ordering::Relaxed) => id.clone(),
                    _ => {
                        if let Some(id) = &old_id {
                            store.remove(id);
                        }
                        new_id()
                    }
                };
                store.save(&id, &data, self.ttl);
                resp.with_cookie(self.cookie(self.key.sign_value(&self.cookie_name, &id)))
            }
        }
    }
}

impl Sessions {
    fn into_parts(self) -> (Arc<Shared>, String) {
        let shared = Arc::new(Shared {
            key: self.key,
            cookie_name: self.cookie_name,
            ttl: self.ttl,
            secure: self.secure,
            same_site: self.same_site,
            store: self.store,
        });
        (shared, self.prefix)
    }
}

fn middleware(shared: Arc<Shared>) -> impl Fn(HandlerFn) -> HandlerFn + Send + Sync + 'static {
    move |next: HandlerFn| -> HandlerFn {
        let shared = shared.clone();
        Arc::new(move |req: Request| {
            let (data, id) = shared.load(&req);
            let st = Arc::new(State {
                data: Mutex::new(data),
                id: Mutex::new(id),
                changed: AtomicBool::new(false),
                destroyed: AtomicBool::new(false),
                new_id: AtomicBool::new(false),
            });
            req.extensions().insert_arc(st.clone());
            let fut = next(req);
            let shared = shared.clone();
            Box::pin(async move { shared.finish(fut.await, &st) })
        })
    }
}

impl Plugin for Sessions {
    fn apply(self, app: App) -> App {
        let (shared, prefix) = self.into_parts();
        app.wrap_on(&prefix, middleware(shared))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn handler() -> HandlerFn {
        Arc::new(|req: Request| {
            let s = Session::from_request(&req).unwrap();
            let r = match req.path() {
                "/login" => {
                    s.insert("user", "alice");
                    s.cycle_id();
                    Response::text("in")
                }
                "/logout" => {
                    s.destroy();
                    Response::text("out")
                }
                _ => Response::text(s.get::<String>("user").unwrap_or_default()),
            };
            Box::pin(std::future::ready(r))
        })
    }

    fn call(app_mw: &dyn Fn(HandlerFn) -> HandlerFn, path: &str, cookie: Option<&str>) -> Response {
        let headers = cookie
            .map(|c| vec![(Bytes::from_static(b"cookie"), Bytes::from(c.to_string()))])
            .unwrap_or_default();
        let f = app_mw(handler());
        futures::executor::block_on(f(Request::new(
            "GET",
            path.to_string(),
            headers,
            Bytes::new(),
        )))
    }

    fn set_cookie(r: &Response) -> String {
        let v = std::str::from_utf8(r.header("set-cookie").unwrap()).unwrap();
        v.split(';').next().unwrap().to_string()
    }

    fn mw(s: Sessions) -> impl Fn(HandlerFn) -> HandlerFn {
        middleware(s.into_parts().0)
    }

    #[test]
    fn cookie_sessions() {
        let mw = mw(Sessions::new(Key::generate()));
        let r = call(&mw, "/login", None);
        let c = set_cookie(&r);
        assert!(!c.contains("alice"), "encrypted");
        assert_eq!(
            call(&mw, "/me", Some(&c)).body(),
            &Bytes::from_static(b"alice")
        );
        assert!(
            call(&mw, "/me", Some(&c)).header("set-cookie").is_none(),
            "unchanged: no cookie"
        );
        assert_eq!(
            call(&mw, "/me", Some("veil_session=garbage")).body(),
            &Bytes::new()
        );
        let out = call(&mw, "/logout", Some(&c));
        assert!(std::str::from_utf8(out.header("set-cookie").unwrap())
            .unwrap()
            .contains("Max-Age=0"));
    }

    #[test]
    fn store_sessions_cycle_ids() {
        let mw = mw(Sessions::new(Key::generate()).store(MemoryStore::new()));
        let c1 = set_cookie(&call(&mw, "/login", None));
        assert_eq!(
            call(&mw, "/me", Some(&c1)).body(),
            &Bytes::from_static(b"alice")
        );
        // ログインし直すと ID が変わり、古い ID は使えない
        let c2 = set_cookie(&call(&mw, "/login", Some(&c1)));
        assert_ne!(c1, c2);
        assert_eq!(call(&mw, "/me", Some(&c1)).body(), &Bytes::new());
        assert_eq!(
            call(&mw, "/me", Some(&c2)).body(),
            &Bytes::from_static(b"alice")
        );
        // 署名の無い ID は受けない
        let id = c2.split_once('.').unwrap().1;
        assert_eq!(
            call(&mw, "/me", Some(&format!("veil_session=AAAA.{}", id))).body(),
            &Bytes::new()
        );
        call(&mw, "/logout", Some(&c2));
        assert_eq!(call(&mw, "/me", Some(&c2)).body(), &Bytes::new());
    }
}
