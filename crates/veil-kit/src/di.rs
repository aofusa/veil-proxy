// F-267: 動的な依存性注入。型ごとのスロット（提供者は ArcSwap で差し替えられる）を登録時に作り、
// 起動（App::with）で凍結する。要求ごとはロックの無い表の参照・ArcSwap::load・提供者の呼び出しだけ。
//! Dependency injection.
//!
//! Register providers by type with a lifetime, install the container with `App::with`, and take dependencies as
//! handler parameters with [`Dep<T>`]:
//!
//! ```no_run
//! use std::sync::Arc;
//! use veil::prelude::*;
//! use veil_kit::di::{Container, Dep, Resolver};
//!
//! struct Pool { url: String }
//! struct Repo { pool: Arc<Pool>, user: String }
//!
//! #[veil::route(path = "/items", method = "GET")]
//! async fn items(repo: Dep<Repo>) -> String {
//!     format!("{} for {}", repo.pool.url, repo.user)
//! }
//!
//! let di = Container::new()
//!     .singleton(|_r: Resolver| async { Ok(Pool { url: "postgres://db".into() }) })
//!     .scoped(|r: Resolver| async move {
//!         let pool = r.get::<Pool>().await?;
//!         let user = r.request().and_then(|q| q.header("x-user")).unwrap_or("anonymous").to_string();
//!         Ok(Repo { pool, user })
//!     });
//! App::new().with(di).run().unwrap();
//! ```
//!
//! - `singleton`: created on first use, then shared by every request (a [`Container::value`] is created up front).
//! - `scoped`: created once per request and shared within it.
//! - `transient`: created every time it is asked for.
//!
//! Providers can be replaced while the server runs ([`Container::replace`], also used in tests like FastAPI's
//! `dependency_overrides`); requests in progress finish with the old provider. Dependency cycles are reported as
//! an error the first time the cycle is resolved.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwap;
use veil::api::{App, FromRequestAsync, Plugin, Request, Response};

type AnyArc = Arc<dyn Any + Send + Sync>;
type ProviderFuture = Pin<Box<dyn Future<Output = Result<AnyArc, DiError>>>>;
type Provider = dyn Fn(Resolver) -> ProviderFuture + Send + Sync;

/// How long a provided value lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    /// One value for the whole process.
    Singleton,
    /// One value per request.
    Scoped,
    /// A new value every time.
    Transient,
}

/// A dependency could not be provided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiError {
    /// No provider for the type.
    Missing(&'static str),
    /// The types depend on each other in a cycle (listed in resolution order).
    Cycle(Vec<&'static str>),
    /// A scoped value was asked for outside a request.
    NoRequest(&'static str),
    /// The provider failed.
    Provider(String),
}

impl std::fmt::Display for DiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiError::Missing(t) => write!(f, "no provider registered for {}", t),
            DiError::Cycle(c) => write!(f, "dependency cycle: {}", c.join(" -> ")),
            DiError::NoRequest(t) => write!(
                f,
                "{} is request-scoped and was asked for outside a request",
                t
            ),
            DiError::Provider(e) => write!(f, "provider failed: {}", e),
        }
    }
}

impl std::error::Error for DiError {}

impl From<String> for DiError {
    fn from(s: String) -> Self {
        DiError::Provider(s)
    }
}

impl From<&str> for DiError {
    fn from(s: &str) -> Self {
        DiError::Provider(s.to_string())
    }
}

impl From<DiError> for Response {
    fn from(e: DiError) -> Response {
        ftlog::error!("dependency injection: {}", e);
        Response::error(500, "Internal Server Error")
    }
}

struct Slot {
    name: &'static str,
    lifetime: Lifetime,
    provider: ArcSwap<Box<Provider>>,
    /// The singleton's value (cleared by `replace`).
    value: Mutex<Option<AnyArc>>,
}

/// A set of providers (cheap to clone; clones share the providers).
#[derive(Clone, Default)]
pub struct Container {
    slots: Arc<HashMap<TypeId, Arc<Slot>>>,
}

impl std::fmt::Debug for Container {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<_> = self.slots.values().map(|s| (s.name, s.lifetime)).collect();
        names.sort_by_key(|n| n.0);
        f.debug_struct("Container")
            .field("providers", &names)
            .finish()
    }
}

/// The container installed with `App::with` (for [`Dep`]).
static INSTALLED: OnceLock<Container> = OnceLock::new();

fn boxed<T, F, Fut>(f: F) -> Box<Provider>
where
    T: Send + Sync + 'static,
    F: Fn(Resolver) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<T, DiError>> + 'static,
{
    Box::new(move |r: Resolver| -> ProviderFuture {
        let fut = f(r);
        Box::pin(async move { fut.await.map(|v| Arc::new(v) as AnyArc) })
    })
}

impl Container {
    /// An empty container.
    pub fn new() -> Container {
        Container::default()
    }

    fn register(
        mut self,
        id: TypeId,
        name: &'static str,
        lifetime: Lifetime,
        p: Box<Provider>,
        value: Option<AnyArc>,
    ) -> Container {
        let slots = Arc::get_mut(&mut self.slots)
            .expect("providers are registered before the container is cloned or installed");
        slots.insert(
            id,
            Arc::new(Slot {
                name,
                lifetime,
                provider: ArcSwap::from_pointee(p),
                value: Mutex::new(value),
            }),
        );
        self
    }

    /// A singleton that already exists.
    pub fn value<T: Send + Sync + 'static>(self, v: T) -> Container {
        let v: AnyArc = Arc::new(v);
        let name = std::any::type_name::<T>();
        let p: Box<Provider> = Box::new(move |_r| -> ProviderFuture {
            Box::pin(std::future::ready(Err(DiError::Missing(name))))
        });
        self.register(TypeId::of::<T>(), name, Lifetime::Singleton, p, Some(v))
    }

    /// A singleton created by `f` on first use.
    pub fn singleton<T, F, Fut>(self, f: F) -> Container
    where
        T: Send + Sync + 'static,
        F: Fn(Resolver) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, DiError>> + 'static,
    {
        self.register(
            TypeId::of::<T>(),
            std::any::type_name::<T>(),
            Lifetime::Singleton,
            boxed(f),
            None,
        )
    }

    /// A value created by `f` once per request.
    pub fn scoped<T, F, Fut>(self, f: F) -> Container
    where
        T: Send + Sync + 'static,
        F: Fn(Resolver) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, DiError>> + 'static,
    {
        self.register(
            TypeId::of::<T>(),
            std::any::type_name::<T>(),
            Lifetime::Scoped,
            boxed(f),
            None,
        )
    }

    /// A value created by `f` every time.
    pub fn transient<T, F, Fut>(self, f: F) -> Container
    where
        T: Send + Sync + 'static,
        F: Fn(Resolver) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, DiError>> + 'static,
    {
        self.register(
            TypeId::of::<T>(),
            std::any::type_name::<T>(),
            Lifetime::Transient,
            boxed(f),
            None,
        )
    }

    /// Replaces the provider of `T` (its lifetime stays; a singleton is created again on next use). Requests in
    /// progress keep the old provider. `Err` when `T` was never registered.
    pub fn replace<T, F, Fut>(&self, f: F) -> Result<(), DiError>
    where
        T: Send + Sync + 'static,
        F: Fn(Resolver) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, DiError>> + 'static,
    {
        let slot = self
            .slots
            .get(&TypeId::of::<T>())
            .ok_or(DiError::Missing(std::any::type_name::<T>()))?;
        slot.provider.store(Arc::new(boxed(f)));
        *slot.value.lock().unwrap() = None;
        Ok(())
    }

    /// Replaces a singleton with an existing value.
    pub fn replace_value<T: Send + Sync + 'static>(&self, v: T) -> Result<(), DiError> {
        let slot = self
            .slots
            .get(&TypeId::of::<T>())
            .ok_or(DiError::Missing(std::any::type_name::<T>()))?;
        *slot.value.lock().unwrap() = Some(Arc::new(v));
        Ok(())
    }

    /// A resolver for one request.
    pub fn resolver(&self, req: &Request) -> Resolver {
        // 要求の複製と元の要求で scoped の値を共有する（拡張領域を先に作る）
        let _ = req.extensions().share();
        Resolver {
            container: self.clone(),
            req: Some(req.clone()),
            stack: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// A resolver outside any request (singletons and transients only).
    pub fn root(&self) -> Resolver {
        Resolver {
            container: self.clone(),
            req: None,
            stack: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// The container installed with `App::with` (`None` before startup).
    pub fn installed() -> Option<&'static Container> {
        INSTALLED.get()
    }
}

impl Plugin for Container {
    fn apply(self, app: App) -> App {
        if INSTALLED.set(self.clone()).is_err() {
            panic!("a veil_kit::di::Container was already installed (install one container per process)");
        }
        app.state(self)
    }
}

/// Values created for one request (in the request's extensions).
#[derive(Default)]
struct ScopedValues(Mutex<Vec<(TypeId, AnyArc)>>);

/// Resolves dependencies inside a provider (and for [`Dep`]).
#[derive(Clone)]
pub struct Resolver {
    container: Container,
    req: Option<Request>,
    stack: Rc<RefCell<Vec<(TypeId, &'static str)>>>,
}

impl Resolver {
    /// The request being served (`None` outside a request).
    pub fn request(&self) -> Option<&Request> {
        self.req.as_ref()
    }

    /// Runs an extractor on the request (for example `Auth<Principal>` inside a scoped provider).
    pub async fn extract<E: FromRequestAsync>(&self) -> Result<E, DiError> {
        let req = self
            .req
            .as_ref()
            .ok_or(DiError::NoRequest(std::any::type_name::<E>()))?;
        E::from_request_async(req)
            .await
            .map_err(|r| DiError::Provider(format!("extractor answered {}", r.status_code())))
    }

    /// The value of `T`.
    pub async fn get<T: Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError> {
        let v = self
            .get_any(TypeId::of::<T>(), std::any::type_name::<T>())
            .await?;
        v.downcast::<T>().map_err(|_| {
            DiError::Provider(format!(
                "provider of {} returned another type",
                std::any::type_name::<T>()
            ))
        })
    }

    async fn get_any(&self, id: TypeId, name: &'static str) -> Result<AnyArc, DiError> {
        let slot = self
            .container
            .slots
            .get(&id)
            .ok_or(DiError::Missing(name))?
            .clone();
        match slot.lifetime {
            Lifetime::Singleton => {
                if let Some(v) = slot.value.lock().unwrap().clone() {
                    return Ok(v);
                }
                let v = self.call(&slot, id).await?;
                // 同時に作られたら先に入った方を使う
                let mut g = slot.value.lock().unwrap();
                Ok(g.get_or_insert(v).clone())
            }
            Lifetime::Transient => self.call(&slot, id).await,
            Lifetime::Scoped => {
                let req = self.req.as_ref().ok_or(DiError::NoRequest(name))?;
                let scoped = req.extensions().get_or_insert_with(ScopedValues::default);
                if let Some((_, v)) = scoped.0.lock().unwrap().iter().find(|(t, _)| *t == id) {
                    return Ok(v.clone());
                }
                let v = self.call(&slot, id).await?;
                let mut g = scoped.0.lock().unwrap();
                if let Some((_, existing)) = g.iter().find(|(t, _)| *t == id) {
                    return Ok(existing.clone());
                }
                g.push((id, v.clone()));
                Ok(v)
            }
        }
    }

    async fn call(&self, slot: &Slot, id: TypeId) -> Result<AnyArc, DiError> {
        {
            let mut st = self.stack.borrow_mut();
            if st.iter().any(|(t, _)| *t == id) {
                let mut cycle: Vec<&'static str> = st.iter().map(|(_, n)| *n).collect();
                cycle.push(slot.name);
                return Err(DiError::Cycle(cycle));
            }
            st.push((id, slot.name));
        }
        let provider = slot.provider.load_full();
        let r = provider(self.clone()).await;
        self.stack.borrow_mut().retain(|(t, _)| *t != id);
        r
    }
}

/// A dependency provided by the installed [`Container`] (500 when it cannot be provided; the reason is logged).
pub struct Dep<T: Send + Sync + 'static>(pub Arc<T>);

impl<T: Send + Sync + 'static> std::ops::Deref for Dep<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Send + Sync + 'static> Clone for Dep<T> {
    fn clone(&self) -> Self {
        Dep(self.0.clone())
    }
}

impl<T: Send + Sync + 'static> FromRequestAsync for Dep<T> {
    async fn from_request_async(req: &Request) -> Result<Self, Response> {
        let c = INSTALLED.get().ok_or_else(|| {
            Response::from(DiError::Provider(
                "no veil_kit::di::Container is installed (App::with(container))".into(),
            ))
        })?;
        c.resolver(req)
            .get::<T>()
            .await
            .map(Dep)
            .map_err(Response::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn req() -> Request {
        Request::new(
            "GET",
            "/",
            vec![(Bytes::from_static(b"x-user"), Bytes::from_static(b"bob"))],
            Bytes::new(),
        )
    }

    struct Pool(usize);
    struct Repo(Arc<Pool>, String);
    struct Counter(usize);

    #[test]
    fn lifetimes() {
        static MADE: AtomicUsize = AtomicUsize::new(0);
        let c = Container::new()
            .singleton(|_r| async { Ok(Pool(MADE.fetch_add(1, Ordering::SeqCst))) })
            .scoped(|r: Resolver| async move {
                let p = r.get::<Pool>().await?;
                Ok(Repo(
                    p,
                    r.request()
                        .and_then(|q| q.header("x-user"))
                        .unwrap_or("")
                        .to_string(),
                ))
            })
            .transient(|_r| async { Ok(Counter(MADE.fetch_add(100, Ordering::SeqCst))) });
        futures::executor::block_on(async {
            let q = req();
            let r = c.resolver(&q);
            let a = r.get::<Repo>().await.unwrap();
            let b = c.resolver(&q).get::<Repo>().await.unwrap();
            assert!(Arc::ptr_eq(&a, &b), "scoped: one per request");
            assert_eq!(a.1, "bob");
            let other = c.resolver(&req()).get::<Repo>().await.unwrap();
            assert!(!Arc::ptr_eq(&a, &other));
            assert!(Arc::ptr_eq(&a.0, &other.0), "singleton: shared");
            let t1 = r.get::<Counter>().await.unwrap();
            let t2 = r.get::<Counter>().await.unwrap();
            assert_ne!(t1.0, t2.0, "transient: new each time");
            assert!(matches!(
                c.root().get::<Repo>().await,
                Err(DiError::NoRequest(_))
            ));
            assert!(matches!(r.get::<String>().await, Err(DiError::Missing(_))));
        });
    }

    #[test]
    fn replace_and_cycles() {
        struct A;
        struct B;
        let c = Container::new()
            .value(Pool(1))
            .singleton(|r: Resolver| async move {
                r.get::<B>().await?;
                Ok(A)
            })
            .singleton(|r: Resolver| async move {
                r.get::<A>().await?;
                Ok(B)
            });
        futures::executor::block_on(async {
            assert_eq!(c.root().get::<Pool>().await.unwrap().0, 1);
            c.replace_value(Pool(2)).unwrap();
            assert_eq!(c.root().get::<Pool>().await.unwrap().0, 2);
            c.replace::<Pool, _, _>(|_r| async { Ok(Pool(3)) }).unwrap();
            assert_eq!(c.root().get::<Pool>().await.unwrap().0, 3);
            match c.root().get::<A>().await {
                Err(DiError::Cycle(names)) => assert_eq!(names.len(), 3),
                Err(e) => panic!("unexpected {}", e),
                Ok(_) => panic!("expected a cycle"),
            }
            assert!(c
                .replace::<String, _, _>(|_r| async { Ok(String::new()) })
                .is_err());
        });
    }
}
