// F-188
//! Route injection, ordering and conflict checks in library mode.
//!
//! Registered handlers (macros, `App::route`, C and Python) are injected into the configuration as
//! `[[route]]` entries of `type = "Handler"` and stably sorted together with the routes written in TOML,
//! **most specific first**, so that the order of declaration does not matter under first-match routing.
//! Routes with identical conditions, one of which would be unreachable, are a startup error.

use super::handler::HandlerEntry;
use super::pattern::PathPattern;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Turns one handler into a `[[route]]` table.
pub(crate) fn handler_route_table(entry: &HandlerEntry) -> toml::Value {
    let mut cond = toml::Table::new();
    cond.insert(
        "path".into(),
        toml::Value::String(entry.pattern.as_str().to_string()),
    );
    if !entry.methods.is_empty() {
        let mut methods: Vec<toml::Value> = entry
            .methods
            .iter()
            .map(|m| toml::Value::String(m.clone()))
            .collect();
        // GET のルートは HEAD も受ける（応答本文は送らない）。
        if entry.methods.iter().any(|m| m == "GET") && !entry.methods.iter().any(|m| m == "HEAD") {
            methods.push(toml::Value::String("HEAD".into()));
        }
        cond.insert("method".into(), toml::Value::Array(methods));
    }
    let mut action = toml::Table::new();
    action.insert("type".into(), toml::Value::String("Handler".into()));
    action.insert(
        "handler".into(),
        toml::Value::String(entry.name.to_string()),
    );
    let mut route = toml::Table::new();
    route.insert("conditions".into(), toml::Value::Table(cond));
    route.insert("action".into(), toml::Value::Table(action));
    toml::Value::Table(route)
}

/// Injects the handler routes into the `route` array. A handler already referenced by name from TOML
/// (where the user specified conditions or security settings explicitly) is not injected.
pub(crate) fn inject_handler_routes(
    root: &mut toml::Value,
    entries: &[Arc<HandlerEntry>],
) -> Result<(), String> {
    let table = root
        .as_table_mut()
        .ok_or_else(|| "config root must be a table".to_string())?;
    let routes = table
        .entry("route")
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    let routes = routes
        .as_array_mut()
        .ok_or_else(|| "'route' must be an array of tables".to_string())?;
    let referenced: BTreeSet<String> = routes
        .iter()
        .filter_map(|r| {
            let action = r.get("action")?;
            if action.get("type")?.as_str()? == "Handler" {
                Some(action.get("handler")?.as_str()?.to_string())
            } else {
                None
            }
        })
        .collect();
    for e in entries {
        if !referenced.contains(&*e.name) {
            routes.push(handler_route_table(e));
        }
    }
    Ok(())
}

/// The sort key of one route (greater keys are evaluated first).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RouteRank {
    /// Depth (the number of static segments plus parameters). Deeper routes come first, so that a prefix match on
    /// `/api` does not take precedence over `/api/{id}`.
    depth: usize,
    /// Number of static segments (at equal depth, more static segments is more specific).
    statics: usize,
    /// Whether this is an exact match (not a prefix match, wildcard or catch-all).
    exact: bool,
    /// Whether a method condition is present.
    has_method: bool,
    /// Number of other conditions (host, header, query and source_ip).
    other_conditions: usize,
}

/// The normalized form of a route's matching conditions (for conflict checks).
#[derive(Debug, Clone, PartialEq)]
struct RouteKey {
    host: Option<String>,
    path: String,
    methods: Option<BTreeSet<String>>,
    header: Option<toml::Value>,
    query: Option<toml::Value>,
    source_ip: Option<toml::Value>,
}

fn route_rank_and_key(route: &toml::Value) -> Result<(RouteRank, RouteKey), String> {
    let cond = route.get("conditions");
    let get = |k: &str| cond.and_then(|c| c.get(k));
    let is_handler = route
        .get("action")
        .and_then(|a| a.get("type"))
        .and_then(|t| t.as_str())
        == Some("Handler");
    let path = get("path").and_then(|p| p.as_str()).unwrap_or("/");

    let (statics, params, exact, norm_path) = if path.contains('{') {
        let pat = PathPattern::parse(path)?;
        let (st, np, no_catch_all) = pat.specificity();
        (
            st,
            (-np) as usize,
            no_catch_all && is_handler,
            pat.normalized(),
        )
    } else if path == "/" && !is_handler {
        (0, 0, false, "/*".to_string())
    } else if let Some(prefix) = path.strip_suffix("/*") {
        let st = prefix.split('/').filter(|s| !s.is_empty()).count();
        (st, 0, false, format!("{}/*", prefix))
    } else {
        let st = path.split('/').filter(|s| !s.is_empty()).count();
        // TOML の静的パスはセグメント境界の前方一致。ハンドラのパスは完全一致。
        (st, 0, is_handler, path.to_string())
    };

    let methods: Option<BTreeSet<String>> = get("method").and_then(|m| m.as_array()).map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_ascii_uppercase()))
            .collect()
    });
    let host = get("host").and_then(|h| h.as_str()).map(|s| s.to_string());
    let header = get("header").cloned();
    let query = get("query").cloned();
    let source_ip = get("source_ip").cloned();
    let other_conditions = host.is_some() as usize
        + header
            .as_ref()
            .and_then(|h| h.as_table())
            .map(|t| t.len())
            .unwrap_or(0)
        + query
            .as_ref()
            .and_then(|q| q.as_table())
            .map(|t| t.len())
            .unwrap_or(0)
        + source_ip.is_some() as usize;

    Ok((
        RouteRank {
            depth: statics + params,
            statics,
            exact,
            has_method: methods.is_some(),
            other_conditions,
        },
        RouteKey {
            host,
            path: norm_path,
            methods,
            header,
            query,
            source_ip,
        },
    ))
}

fn describe(route: &toml::Value) -> String {
    let cond = route
        .get("conditions")
        .map(|c| c.to_string())
        .unwrap_or_else(|| "{}".into());
    let action = route
        .get("action")
        .map(|a| a.to_string())
        .unwrap_or_default();
    format!("conditions = {} / action = {}", cond.trim(), action.trim())
}

/// Stably sorts the `route` array with the most specific first and detects routes with identical conditions.
pub(crate) fn sort_and_check_routes(root: &mut toml::Value) -> Result<(), String> {
    let Some(routes) = root.get_mut("route").and_then(|r| r.as_array_mut()) else {
        return Ok(());
    };
    let mut ranked = Vec::with_capacity(routes.len());
    for (i, r) in routes.drain(..).enumerate() {
        let (rank, key) = route_rank_and_key(&r).map_err(|e| format!("route[{}]: {}", i, e))?;
        ranked.push((rank, key, r));
    }
    // 衝突検査: 同じ host・正規化パス・header/query/source_ip で、メソッド集合が重なる
    // （両方メソッド無指定、または共通のメソッドがある）なら後者は到達不能。
    for i in 0..ranked.len() {
        for j in (i + 1)..ranked.len() {
            let (a, b) = (&ranked[i].1, &ranked[j].1);
            if a.host == b.host
                && a.path == b.path
                && a.header == b.header
                && a.query == b.query
                && a.source_ip == b.source_ip
            {
                let overlap = match (&a.methods, &b.methods) {
                    (None, None) => true,
                    (Some(x), Some(y)) => x.intersection(y).next().is_some(),
                    _ => false,
                };
                if overlap {
                    return Err(format!(
                        "conflicting routes (identical conditions):\n  - {}\n  - {}",
                        describe(&ranked[i].2),
                        describe(&ranked[j].2)
                    ));
                }
            }
        }
    }
    // 安定ソート（同順位は元の順序を保つ）。
    ranked.sort_by(|x, y| y.0.cmp(&x.0));
    routes.extend(ranked.into_iter().map(|(_, _, r)| r));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(path: &str, methods: Option<&[&str]>, handler: Option<&str>) -> toml::Value {
        let mut cond = toml::Table::new();
        cond.insert("path".into(), path.into());
        if let Some(m) = methods {
            cond.insert(
                "method".into(),
                toml::Value::Array(m.iter().map(|s| toml::Value::from(*s)).collect()),
            );
        }
        let mut action = toml::Table::new();
        match handler {
            Some(h) => {
                action.insert("type".into(), "Handler".into());
                action.insert("handler".into(), h.into());
            }
            None => {
                action.insert("type".into(), "Proxy".into());
                action.insert("url".into(), "http://127.0.0.1:1".into());
            }
        }
        let mut r = toml::Table::new();
        r.insert("conditions".into(), toml::Value::Table(cond));
        r.insert("action".into(), toml::Value::Table(action));
        toml::Value::Table(r)
    }

    fn order(routes: Vec<toml::Value>) -> Result<Vec<String>, String> {
        let mut root = toml::Value::Table(toml::Table::new());
        root.as_table_mut()
            .unwrap()
            .insert("route".into(), toml::Value::Array(routes));
        sort_and_check_routes(&mut root)?;
        Ok(root["route"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let p = r["conditions"]["path"].as_str().unwrap().to_string();
                match r["conditions"].get("method") {
                    Some(m) => format!("{} {}", m, p),
                    None => p,
                }
            })
            .collect())
    }

    #[test]
    fn more_specific_routes_come_first_regardless_of_declaration_order() {
        let got = order(vec![
            route("/", None, None),
            route("/users/{id}", Some(&["GET"]), Some("get_user")),
            route("/users", Some(&["POST"]), Some("create_user")),
            route("/users/me", Some(&["GET"]), Some("me")),
            route("/files/{*rest}", Some(&["GET"]), Some("files")),
            route("/api/*", None, None),
        ])
        .unwrap();
        assert_eq!(
            got,
            vec![
                r#"["GET"] /users/me"#,
                r#"["GET"] /users/{id}"#,
                r#"["POST"] /users"#,
                r#"["GET"] /files/{*rest}"#,
                "/api/*",
                "/",
            ]
        );
    }

    #[test]
    fn toml_prefix_route_does_not_shadow_deeper_handler() {
        let got = order(vec![
            route("/api", None, None),
            route("/api/{id}", Some(&["GET"]), Some("item")),
        ])
        .unwrap();
        assert_eq!(got, vec![r#"["GET"] /api/{id}"#, "/api"]);
    }

    #[test]
    fn method_specific_before_any_method_on_same_path() {
        let got = order(vec![
            route("/x", None, Some("any")),
            route("/x", Some(&["GET"]), Some("get")),
        ])
        .unwrap();
        assert_eq!(got, vec![r#"["GET"] /x"#, "/x"]);
    }

    #[test]
    fn identical_conditions_conflict() {
        let err = order(vec![
            route("/users/{id}", Some(&["GET"]), Some("a")),
            route("/users/{uid}", Some(&["GET", "POST"]), Some("b")),
        ])
        .unwrap_err();
        assert!(err.contains("conflicting routes"), "{}", err);
        // メソッドが重ならなければ衝突しない
        order(vec![
            route("/users/{id}", Some(&["GET"]), Some("a")),
            route("/users/{id}", Some(&["DELETE"]), Some("b")),
        ])
        .unwrap();
    }
}
