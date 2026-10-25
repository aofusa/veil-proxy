//! 待ち受け（`[[listener]]`）の設定の型・正規化・検証（F-260）
//!
//! `[server] listen`・`listen_cleartext`（と `[http3]`）は「暗黙のリスナー」へ正規化し、`[[listener]]` と同じ
//! 列（`ListenerSpec`）にする。起動・リロード・検証はこの列だけを見る。

use super::*;
use crate::listen_addr::ListenAddr;
use std::net::SocketAddr;

/// `[[listener]]` の 1 個（設定の型）。
///
/// キーの打ち間違いを黙って既定値にしないよう、知らないキーはエラーにする（新しい表なので互換の心配が無い）。
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfigSection {
    /// ログ・エラー・`redirect_to` で使う名前（省略時は `listener-<[[listener]] の添字>`）
    #[serde(default)]
    pub name: Option<String>,
    /// TCP の `host:port` か `unix:<path>`
    pub listen: String,
    /// TLS で受けるか（必須。`[tls]` の証明書を使う）
    pub tls: bool,
    /// ALPN に h2 を出すか（TLS のみ。既定は `[server] http2_enabled`）
    #[serde(default)]
    pub http2: Option<bool>,
    /// 同じポートの UDP で HTTP/3 を受けるか（TLS かつ TCP のときだけ）
    #[serde(default)]
    pub http3: bool,
    /// HTTP/3 の UDP を別のアドレスにするとき
    #[serde(default)]
    pub http3_listen: Option<String>,
    /// 平文 HTTP/1.1 を受けるか（平文のみ。既定 true）
    #[serde(default)]
    pub allow_http: Option<bool>,
    /// h2c を受けるか（平文のみ。既定 true）
    #[serde(default)]
    pub allow_h2c: Option<bool>,
    /// 受けた要求を https へ 301 で返すか（平文のみ。既定 false。`[server] enable_secure_redirect` の既定と違う）
    #[serde(default)]
    pub secure_redirect: Option<bool>,
    /// リダイレクト先のポートを取る TLS のリスナー名（既定: 最初の TLS のリスナー）
    #[serde(default)]
    pub redirect_to: Option<String>,
    /// `unix:<path>` のソケットファイルのパーミッション（既定は `[server] unix_socket_permissions`）
    #[serde(default)]
    pub unix_socket_permissions: Option<String>,
    /// このリスナーのワーカー数（既定は `[server] threads`）
    #[serde(default)]
    pub threads: Option<usize>,
}

/// 正規化したリスナー 1 個（`[server] listen`・`listen_cleartext` の暗黙のリスナーも同じ形）。
#[derive(Clone, Debug, PartialEq)]
pub struct ListenerSpec {
    /// 名前（暗黙のリスナーは `"tls"`・`"cleartext"`）
    pub name: String,
    /// 設定に書かれた待ち受けのアドレス（表示用）
    pub listen: String,
    pub addr: ListenAddr,
    pub tls: bool,
    /// TLS で ALPN に h2 を出すか（平文では使わない）
    pub http2: bool,
    /// HTTP/3 の UDP アドレス（TLS かつ `http3` のとき）
    pub http3: Option<SocketAddr>,
    /// 平文の受け入れ方（TLS のリスナーでは使わない）
    pub cleartext: CleartextPolicy,
    pub unix_socket_permissions: String,
    /// このリスナーのワーカー数（`None` は `[server] threads`）
    pub threads: Option<usize>,
}

impl ListenerSpec {
    /// TLS の ALPN のプロファイル（TLS のリスナーのとき）
    pub fn alpn_profile(&self) -> Option<crate::tls_reload::AlpnProfile> {
        self.tls
            .then(|| crate::tls_reload::AlpnProfile::from_http2(self.http2))
    }

    /// ワーカー数（`threads` が無い・0 なら `default`）
    pub fn worker_count(&self, default: usize) -> usize {
        match self.threads {
            Some(n) if n > 0 => n,
            _ => default,
        }
    }

    /// 起動時のログの 1 行（`listener "https" 0.0.0.0:8443 tls alpn=h2,http/1.1 http3=0.0.0.0:8443 workers=N`）
    pub fn describe(&self, workers: usize) -> String {
        let mut s = format!("listener \"{}\" {}", self.name, self.addr);
        if self.tls {
            s.push_str(if self.http2 {
                " tls alpn=h2,http/1.1"
            } else {
                " tls alpn=none(http/1.1)"
            });
            if let Some(h3) = self.http3 {
                s.push_str(&format!(" http3={}", h3));
            }
        } else {
            let p = &self.cleartext;
            s.push_str(&format!(
                " cleartext http1={} h2c={}",
                if p.allow_http { "on" } else { "off" },
                if p.allow_h2c { "on" } else { "off" }
            ));
            if p.secure_redirect {
                s.push_str(&format!(" redirect=https:{}", p.redirect_port));
            }
        }
        s.push_str(&format!(" workers={}", workers));
        s
    }
}

/// 暗黙の TLS のリスナー（`[server] listen`）の名前
pub const IMPLICIT_TLS_LISTENER: &str = "tls";
/// 暗黙の平文のリスナー（`[server] listen_cleartext`）の名前
pub const IMPLICIT_CLEARTEXT_LISTENER: &str = "cleartext";

/// TCP のアドレスが重なるか（同じポートで、IP が同じか片方がワイルドカード）。
///
/// `SO_REUSEPORT` で同じポートを 2 回開くとエラーにならずに接続が分かれるので、設定で必ず止める。
/// `[::]` は（デュアルスタックで）IPv4 とも重なるとみなす。
pub(super) fn socket_addrs_overlap(a: &SocketAddr, b: &SocketAddr) -> bool {
    if a.port() != b.port() {
        return false;
    }
    let (ia, ib) = (a.ip(), b.ip());
    if ia == ib {
        return true;
    }
    let wild_covers = |w: &std::net::IpAddr, o: &std::net::IpAddr| {
        w.is_unspecified() && (w.is_ipv6() || w.is_ipv4() == o.is_ipv4())
    };
    wild_covers(&ia, &ib) || wild_covers(&ib, &ia)
}

fn listen_addrs_overlap(a: &ListenAddr, b: &ListenAddr) -> bool {
    match (a, b) {
        (ListenAddr::Tcp(x), ListenAddr::Tcp(y)) => socket_addrs_overlap(x, y),
        #[cfg(unix)]
        (ListenAddr::Unix(x), ListenAddr::Unix(y)) => x == y,
        #[cfg(unix)]
        _ => false,
    }
}

fn invalid<T>(m: String) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, m))
}

fn parse_addr(key: &str, v: &str) -> io::Result<ListenAddr> {
    ListenAddr::parse(v).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Invalid {}: {}", key, e),
        )
    })
}

/// HTTP/3 の UDP アドレスを決める（`http3_listen` が無ければ TCP の `listen` と同じ）。
fn http3_udp_addr(key: &str, explicit: Option<&str>, tcp: &ListenAddr) -> io::Result<SocketAddr> {
    match explicit {
        Some(s) => s.parse::<SocketAddr>().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Invalid {}: '{}': {}", key, s, e),
            )
        }),
        None => match tcp.tcp_addr() {
            Some(a) => Ok(a),
            None => invalid(format!(
                "{} is needed: HTTP/3 uses UDP, and the TLS listener {} is a unix socket",
                key, tcp
            )),
        },
    }
}

/// 設定の待ち受けを正規化したリスナーの列にし、規則を検証する（F-200・F-260）。
///
/// 暗黙のリスナー（`[server] listen` → `"tls"`、`listen_cleartext` → `"cleartext"`）を先頭に置き、続けて
/// `[[listener]]` を書いた順に並べる。添字がリスナーの id（平文の方針の表の添字）になる。
pub(super) fn normalize_listeners(config: &Config) -> io::Result<Vec<ListenerSpec>> {
    let server = &config.server;
    validate_removed_keys(server)?;
    let explicit: &[ListenerConfigSection] = config.listener.as_deref().unwrap_or(&[]);
    if server.listen.is_none() && server.listen_cleartext.is_none() && explicit.is_empty() {
        return invalid(
            "no listener: set [server].listen (TLS) and/or [server].listen_cleartext (plaintext), \
             or add a [[listener]]"
                .into(),
        );
    }

    let mut specs: Vec<ListenerSpec> = Vec::with_capacity(2 + explicit.len());
    // リダイレクト先の名前（解決は全員を並べてから）。`None` は「最初の TLS のリスナー」。
    let mut redirect_names: Vec<Option<String>> = Vec::with_capacity(2 + explicit.len());

    // 暗黙の TLS のリスナー（`[server] listen`、`[http3] enabled/listen`）
    let tls_addr = match &server.listen {
        Some(l) => Some(parse_addr("[server].listen", l)?),
        None => None,
    };
    if config.http3.enabled && tls_addr.is_none() {
        return invalid(
            "[http3].enabled = true needs [server].listen (QUIC requires TLS); with [[listener]], \
             set http3 = true on a TLS listener instead"
                .into(),
        );
    }
    if let (Some(l), Some(addr)) = (&server.listen, &tls_addr) {
        let http3 = if config.http3.enabled {
            Some(http3_udp_addr(
                "[http3].listen",
                config.http3.listen.as_deref(),
                addr,
            )?)
        } else {
            None
        };
        specs.push(ListenerSpec {
            name: IMPLICIT_TLS_LISTENER.into(),
            listen: l.clone(),
            addr: addr.clone(),
            tls: true,
            http2: server.http2_enabled,
            http3,
            cleartext: CleartextPolicy::default(),
            unix_socket_permissions: server.unix_socket_permissions.clone(),
            threads: server.threads,
        });
        redirect_names.push(None);
    }

    // 暗黙の平文のリスナー（`[server] listen_cleartext`）
    if let Some(c) = &server.listen_cleartext {
        let addr = parse_addr("[server].listen_cleartext", c)?;
        if Some(&addr) == tls_addr.as_ref() {
            return invalid(format!(
                "[server].listen and [server].listen_cleartext must differ ({})",
                c
            ));
        }
        if !server.allow_http && !server.allow_h2c {
            return invalid(
                "[server].listen_cleartext accepts nothing: allow_http and allow_h2c are both false"
                    .into(),
            );
        }
        if server.enable_secure_redirect && server.listen.is_none() {
            return invalid(
                "[server].enable_secure_redirect = true needs a TLS listener ([server].listen) to \
                 redirect to; set enable_secure_redirect = false to serve plaintext"
                    .into(),
            );
        }
        specs.push(ListenerSpec {
            name: IMPLICIT_CLEARTEXT_LISTENER.into(),
            listen: c.clone(),
            addr,
            tls: false,
            http2: false,
            http3: None,
            cleartext: CleartextPolicy::from_server(server),
            unix_socket_permissions: server.unix_socket_permissions.clone(),
            threads: server.threads,
        });
        // 暗黙の平文のリスナーは暗黙の TLS のリスナー（先頭）へ返す（`[server].listen` のポート。今までと同じ）
        redirect_names.push(None);
    }

    // `[[listener]]`
    for (i, l) in explicit.iter().enumerate() {
        let name = match &l.name {
            Some(n) if n.trim().is_empty() => {
                return invalid(format!("[[listener]] #{}: name must not be empty", i))
            }
            Some(n) => n.clone(),
            None => format!("listener-{}", i),
        };
        let key = format!("listener \"{}\"", name);
        let addr = parse_addr(&format!("{} listen", key), &l.listen)?;
        let http3 = if l.http3 {
            if !l.tls {
                return invalid(format!(
                    "{}: http3 = true needs tls = true (QUIC requires TLS)",
                    key
                ));
            }
            Some(http3_udp_addr(
                &format!("{} http3_listen", key),
                l.http3_listen.as_deref(),
                &addr,
            )?)
        } else {
            if l.http3_listen.is_some() {
                return invalid(format!("{}: http3_listen needs http3 = true", key));
            }
            None
        };
        let cleartext = if l.tls {
            // 平文だけのキーを TLS のリスナーに書いたら、効かないことを黙らない
            for (k, set) in [
                ("allow_http", l.allow_http.is_some()),
                ("allow_h2c", l.allow_h2c.is_some()),
                ("secure_redirect", l.secure_redirect.is_some()),
                ("redirect_to", l.redirect_to.is_some()),
            ] {
                if set {
                    return invalid(format!(
                        "{}: {} applies to plaintext listeners (tls = false) only",
                        key, k
                    ));
                }
            }
            CleartextPolicy::default()
        } else {
            if l.http2.is_some() {
                return invalid(format!(
                    "{}: http2 applies to TLS listeners only; a plaintext listener accepts h2c with allow_h2c",
                    key
                ));
            }
            let p = CleartextPolicy {
                allow_http: l.allow_http.unwrap_or(true),
                allow_h2c: l.allow_h2c.unwrap_or(true),
                // F-260: `[[listener]]` は 1 個ずつ明示して書くので、平文のリスナーは既定で平文のまま配る
                secure_redirect: l.secure_redirect.unwrap_or(false),
                redirect_port: 443,
            };
            if !p.allow_http && !p.allow_h2c {
                return invalid(format!(
                    "{} accepts nothing: allow_http and allow_h2c are both false",
                    key
                ));
            }
            if l.redirect_to.is_some() && !p.secure_redirect {
                return invalid(format!("{}: redirect_to needs secure_redirect = true", key));
            }
            p
        };
        if let Some(p) = &l.unix_socket_permissions {
            parse_unix_socket_permissions(p).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("{}: {}", key, e))
            })?;
        }
        specs.push(ListenerSpec {
            name,
            listen: l.listen.clone(),
            addr,
            tls: l.tls,
            http2: l.http2.unwrap_or(server.http2_enabled),
            http3,
            cleartext,
            unix_socket_permissions: l
                .unix_socket_permissions
                .clone()
                .unwrap_or_else(|| server.unix_socket_permissions.clone()),
            threads: l.threads.or(server.threads),
        });
        redirect_names.push(l.redirect_to.clone());
    }

    // 名前の重複
    for (i, a) in specs.iter().enumerate() {
        if specs[..i].iter().any(|b| b.name == a.name) {
            return invalid(format!(
                "duplicate listener name \"{}\" (the [server].listen and [server].listen_cleartext \
                 listeners are named \"{}\" and \"{}\")",
                a.name, IMPLICIT_TLS_LISTENER, IMPLICIT_CLEARTEXT_LISTENER
            ));
        }
    }

    // リダイレクト先のポート
    let first_tls = specs.iter().position(|s| s.tls);
    for i in 0..specs.len() {
        if specs[i].tls || !specs[i].cleartext.secure_redirect {
            continue;
        }
        let target = match &redirect_names[i] {
            Some(n) => match specs.iter().position(|s| &s.name == n) {
                Some(t) if specs[t].tls => t,
                Some(_) => {
                    return invalid(format!(
                        "listener \"{}\": redirect_to \"{}\" is not a TLS listener",
                        specs[i].name, n
                    ))
                }
                None => {
                    return invalid(format!(
                        "listener \"{}\": redirect_to \"{}\" names no listener",
                        specs[i].name, n
                    ))
                }
            },
            None => match first_tls {
                Some(t) => t,
                None => {
                    return invalid(format!(
                    "listener \"{}\": secure_redirect = true needs a TLS listener to redirect to; \
                         set secure_redirect = false to serve plaintext",
                    specs[i].name
                ))
                }
            },
        };
        // UDS にはポートが無いので 443（今までと同じ）
        specs[i].cleartext.redirect_port = specs[target].addr.tcp_addr().map_or(443, |a| a.port());
    }

    // h2c は http2 feature が要る（今までは起動時のエラーだった。`-t` で分かるようにする）
    #[cfg(not(feature = "http2"))]
    if let Some(s) = specs.iter().find(|s| !s.tls) {
        return invalid(format!(
            "listener \"{}\": plaintext listeners need a build with the http2 feature",
            s.name
        ));
    }

    check_address_conflicts(config, &specs)?;

    if specs.iter().any(|s| s.tls) && !config.tls.is_configured() {
        return invalid(
            "TLS certificate is required: set [tls] cert_path and key_path \
             ([server].listen, TLS [[listener]]s and HTTP/3 are TLS listeners; a plaintext-only server \
             uses only plaintext listeners)"
                .into(),
        );
    }
    Ok(specs)
}

/// リスナー同士・HTTP/3 の UDP・`[[l4]]`・`[acme] challenge_listen` のアドレスの重なりを止める（F-260）。
fn check_address_conflicts(config: &Config, specs: &[ListenerSpec]) -> io::Result<()> {
    // (表示名, アドレス)
    #[cfg_attr(not(any(feature = "l4-proxy", feature = "acme")), allow(unused_mut))]
    let mut tcp: Vec<(String, ListenAddr)> = specs
        .iter()
        .map(|s| (format!("listener \"{}\"", s.name), s.addr.clone()))
        .collect();
    #[cfg_attr(not(feature = "l4-proxy"), allow(unused_mut))]
    let mut udp: Vec<(String, SocketAddr)> = specs
        .iter()
        .filter_map(|s| {
            s.http3
                .map(|a| (format!("listener \"{}\" (HTTP/3)", s.name), a))
        })
        .collect();
    #[cfg(feature = "l4-proxy")]
    for l4 in config.l4.iter().flatten() {
        // 解析できない `listen` は L4 の起動側がエラーにする
        if let Ok(a) = l4.listen.parse::<SocketAddr>() {
            let label = format!("[[l4]] \"{}\"", l4.name);
            match l4.protocol {
                L4Protocol::Tcp => tcp.push((label, ListenAddr::Tcp(a))),
                L4Protocol::Udp => udp.push((label, a)),
            }
        }
    }
    #[cfg(feature = "acme")]
    if config.acme.enabled && !config.acme.challenge_listen.is_empty() {
        if let Ok(a) = config.acme.challenge_listen.parse::<SocketAddr>() {
            tcp.push(("[acme] challenge_listen".into(), ListenAddr::Tcp(a)));
        }
    }
    #[cfg(not(any(feature = "l4-proxy", feature = "acme")))]
    let _ = config;
    for i in 0..tcp.len() {
        for j in 0..i {
            if listen_addrs_overlap(&tcp[i].1, &tcp[j].1) {
                return invalid(format!(
                    "{} ({}) and {} ({}) listen on the same address",
                    tcp[j].0, tcp[j].1, tcp[i].0, tcp[i].1
                ));
            }
        }
    }
    for i in 0..udp.len() {
        for j in 0..i {
            if socket_addrs_overlap(&udp[i].1, &udp[j].1) {
                return invalid(format!(
                    "{} ({}) and {} ({}) listen on the same UDP address",
                    udp[j].0, udp[j].1, udp[i].0, udp[i].1
                ));
            }
        }
    }
    Ok(())
}

/// 設定に HTTP/3 のリスナーがあるか（`[http3] enabled` か `[[listener]] http3 = true`）
pub(super) fn config_has_http3(config: &Config) -> bool {
    config.http3.enabled || config.listener.iter().flatten().any(|l| l.http3)
}

/// リロードで「同じリスナーの列か」を比べる鍵（名前・アドレス・TLS か）。平文の方針の表の添字が同じ意味かの判定。
pub(super) fn listener_identity(specs: &[ListenerSpec]) -> Vec<(String, ListenAddr, bool)> {
    specs
        .iter()
        .map(|s| (s.name.clone(), s.addr.clone(), s.tls))
        .collect()
}

/// 平文の方針の表（リスナーの id で引く）
pub(super) fn cleartext_table(specs: &[ListenerSpec]) -> Arc<[CleartextPolicy]> {
    specs.iter().map(|s| s.cleartext).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TLS: &str = "[tls]\ncert_path = \"/c.pem\"\nkey_path = \"/k.pem\"\n";

    fn specs(toml_text: &str) -> Result<Vec<ListenerSpec>, String> {
        let cfg: Config = toml::from_str(toml_text).map_err(|e| e.to_string())?;
        normalize_listeners(&cfg).map_err(|e| e.to_string())
    }

    fn err(toml_text: &str) -> String {
        specs(toml_text).unwrap_err()
    }

    #[test]
    fn legacy_keys_become_implicit_listeners() {
        let s = specs(&format!(
            "[server]\nlisten = \"127.0.0.1:8443\"\nlisten_cleartext = \"127.0.0.1:8080\"\nthreads = 3\nhttp2_enabled = true\n[http3]\nenabled = true\n{}",
            TLS
        ))
        .unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "tls");
        assert!(s[0].tls && s[0].http2);
        assert_eq!(s[0].http3, Some("127.0.0.1:8443".parse().unwrap()));
        assert_eq!(s[0].threads, Some(3));
        assert_eq!(s[1].name, "cleartext");
        assert!(!s[1].tls);
        // [server] enable_secure_redirect の既定は true のまま、リダイレクト先は listen のポート
        assert!(s[1].cleartext.secure_redirect);
        assert_eq!(s[1].cleartext.redirect_port, 8443);
        assert_eq!(s[1].threads, Some(3));
    }

    #[test]
    fn http3_listen_overrides_udp_address() {
        let s = specs(&format!(
            "[server]\nlisten = \"127.0.0.1:8443\"\n[http3]\nenabled = true\nlisten = \"127.0.0.1:9443\"\n{}",
            TLS
        ))
        .unwrap();
        assert_eq!(s[0].http3, Some("127.0.0.1:9443".parse().unwrap()));
    }

    /// HttpArena と同じ 4 ポートの構成
    const ARENA: &str = r#"
[server]
http2_enabled = true
threads = 2

[[listener]]
name = "plain"
listen = "0.0.0.0:8080"
tls = false

[[listener]]
name = "h1tls"
listen = "0.0.0.0:8081"
tls = true
http2 = false

[[listener]]
name = "h2c"
listen = "0.0.0.0:8082"
tls = false
allow_http = false

[[listener]]
name = "https"
listen = "0.0.0.0:8443"
tls = true
http3 = true
threads = 4
"#;

    #[test]
    fn explicit_listeners_with_defaults() {
        let s = specs(&format!("{}{}", ARENA, TLS)).unwrap();
        assert_eq!(s.len(), 4);
        let by = |n: &str| s.iter().find(|x| x.name == n).unwrap().clone();
        let plain = by("plain");
        assert!(!plain.tls && plain.cleartext.allow_http && plain.cleartext.allow_h2c);
        // [[listener]] の secure_redirect の既定は false
        assert!(!plain.cleartext.secure_redirect);
        let h1 = by("h1tls");
        assert!(h1.tls && !h1.http2);
        assert_eq!(
            h1.alpn_profile(),
            Some(crate::tls_reload::AlpnProfile::H1Only)
        );
        let h2c = by("h2c");
        assert!(!h2c.cleartext.allow_http && h2c.cleartext.allow_h2c);
        let https = by("https");
        // http2 の既定は [server] http2_enabled
        assert!(https.http2);
        assert_eq!(https.http3, Some("0.0.0.0:8443".parse().unwrap()));
        assert_eq!(https.worker_count(8), 4);
        assert_eq!(plain.worker_count(8), 2);
        assert_eq!(by("h2c").alpn_profile(), None);
    }

    #[test]
    fn default_names_use_the_listener_index() {
        let s = specs(
            "[[listener]]\nlisten = \"127.0.0.1:1\"\ntls = false\n[[listener]]\nlisten = \"127.0.0.1:2\"\ntls = false\n",
        )
        .unwrap();
        assert_eq!(s[0].name, "listener-0");
        assert_eq!(s[1].name, "listener-1");
    }

    #[test]
    fn server_section_may_be_omitted() {
        specs("[[listener]]\nlisten = \"127.0.0.1:1\"\ntls = false\n").unwrap();
    }

    #[test]
    fn tls_key_is_required_and_unknown_keys_are_rejected() {
        assert!(err("[[listener]]\nlisten = \"127.0.0.1:1\"\n").contains("tls"));
        assert!(
            err("[[listener]]\nlisten = \"127.0.0.1:1\"\ntls = false\nallow_htp = true\n")
                .contains("allow_htp")
        );
    }

    #[test]
    fn redirect_to_resolves_the_tls_port() {
        let base = format!(
            "[[listener]]\nname = \"a\"\nlisten = \"0.0.0.0:443\"\ntls = true\n\
             [[listener]]\nname = \"b\"\nlisten = \"0.0.0.0:8443\"\ntls = true\n{}",
            TLS
        );
        // 既定は最初の TLS のリスナー
        let s = specs(&format!(
            "{}[[listener]]\nname = \"r\"\nlisten = \"0.0.0.0:80\"\ntls = false\nsecure_redirect = true\n",
            base
        ))
        .unwrap();
        assert_eq!(s[2].cleartext.redirect_port, 443);
        let s = specs(&format!(
            "{}[[listener]]\nname = \"r\"\nlisten = \"0.0.0.0:80\"\ntls = false\nsecure_redirect = true\nredirect_to = \"b\"\n",
            base
        ))
        .unwrap();
        assert_eq!(s[2].cleartext.redirect_port, 8443);
        let e = specs(&format!(
            "{}[[listener]]\nname = \"r\"\nlisten = \"0.0.0.0:80\"\ntls = false\nsecure_redirect = true\nredirect_to = \"nope\"\n",
            base
        ))
        .unwrap_err();
        assert!(e.contains("names no listener"), "{}", e);
        let e = specs(&format!(
            "{}[[listener]]\nname = \"r\"\nlisten = \"0.0.0.0:80\"\ntls = false\nsecure_redirect = true\nredirect_to = \"r\"\n",
            base
        ))
        .unwrap_err();
        assert!(e.contains("not a TLS listener"), "{}", e);
        let e = err("[[listener]]\nlisten = \"0.0.0.0:80\"\ntls = false\nsecure_redirect = true\n");
        assert!(e.contains("needs a TLS listener"), "{}", e);
        let e = err("[[listener]]\nlisten = \"0.0.0.0:80\"\ntls = false\nredirect_to = \"x\"\n");
        assert!(e.contains("needs secure_redirect"), "{}", e);
    }

    #[test]
    fn http3_conditions() {
        let e = err("[[listener]]\nlisten = \"0.0.0.0:80\"\ntls = false\nhttp3 = true\n");
        assert!(e.contains("needs tls = true"), "{}", e);
        let e = err(&format!(
            "[[listener]]\nlisten = \"0.0.0.0:443\"\ntls = true\nhttp3_listen = \"0.0.0.0:443\"\n{}",
            TLS
        ));
        assert!(e.contains("http3_listen needs http3"), "{}", e);
        #[cfg(unix)]
        {
            let e = err(&format!(
                "[[listener]]\nlisten = \"unix:/tmp/x.sock\"\ntls = true\nhttp3 = true\n{}",
                TLS
            ));
            assert!(e.contains("unix socket"), "{}", e);
        }
        // [http3] enabled は [server] listen の暗黙のリスナーにだけ効く
        let e = err(&format!(
            "[[listener]]\nlisten = \"0.0.0.0:443\"\ntls = true\n[http3]\nenabled = true\n{}",
            TLS
        ));
        assert!(e.contains("[http3].enabled"), "{}", e);
    }

    #[test]
    fn keys_for_the_other_kind_are_rejected() {
        let e = err(&format!(
            "[[listener]]\nlisten = \"0.0.0.0:443\"\ntls = true\nallow_http = false\n{}",
            TLS
        ));
        assert!(e.contains("plaintext listeners"), "{}", e);
        let e = err("[[listener]]\nlisten = \"0.0.0.0:80\"\ntls = false\nhttp2 = true\n");
        assert!(e.contains("TLS listeners only"), "{}", e);
        let e = err(
            "[[listener]]\nlisten = \"0.0.0.0:80\"\ntls = false\nallow_http = false\nallow_h2c = false\n",
        );
        assert!(e.contains("accepts nothing"), "{}", e);
    }

    #[test]
    fn tls_listener_needs_certificate() {
        let e = err("[[listener]]\nlisten = \"0.0.0.0:443\"\ntls = true\n");
        assert!(e.contains("TLS certificate is required"), "{}", e);
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let e = err(&format!(
            "[server]\nlisten = \"0.0.0.0:443\"\n[[listener]]\nname = \"tls\"\nlisten = \"0.0.0.0:8443\"\ntls = true\n{}",
            TLS
        ));
        assert!(e.contains("duplicate listener name \"tls\""), "{}", e);
    }

    #[test]
    fn duplicate_addresses_are_rejected() {
        // ワイルドカードと個別のアドレス
        let e = err(
            "[[listener]]\nlisten = \"0.0.0.0:8080\"\ntls = false\n[[listener]]\nlisten = \"127.0.0.1:8080\"\ntls = false\n",
        );
        assert!(e.contains("same address"), "{}", e);
        // [::] は IPv4 とも重なる
        let e = err(
            "[[listener]]\nlisten = \"[::]:8080\"\ntls = false\n[[listener]]\nlisten = \"127.0.0.1:8080\"\ntls = false\n",
        );
        assert!(e.contains("same address"), "{}", e);
        // 別のポート・別の個別アドレスは良い
        specs(
            "[[listener]]\nlisten = \"127.0.0.1:8080\"\ntls = false\n[[listener]]\nlisten = \"127.0.0.2:8080\"\ntls = false\n[[listener]]\nlisten = \"127.0.0.1:8081\"\ntls = false\n",
        )
        .unwrap();
        // [server] と [[listener]]
        let e = err(&format!(
            "[server]\nlisten = \"0.0.0.0:443\"\n[[listener]]\nlisten = \"0.0.0.0:443\"\ntls = true\n{}",
            TLS
        ));
        assert!(e.contains("same address"), "{}", e);
        // HTTP/3 の UDP 同士（TCP は別）
        let e = err(&format!(
            "[[listener]]\nlisten = \"0.0.0.0:443\"\ntls = true\nhttp3 = true\n\
             [[listener]]\nlisten = \"0.0.0.0:8443\"\ntls = true\nhttp3 = true\nhttp3_listen = \"127.0.0.1:443\"\n{}",
            TLS
        ));
        assert!(e.contains("same UDP address"), "{}", e);
    }

    #[cfg(feature = "l4-proxy")]
    #[test]
    fn l4_addresses_conflict_with_listeners() {
        let l4 = |listen: &str, proto: &str| {
            format!(
                "[[listener]]\nlisten = \"0.0.0.0:8443\"\ntls = true\nhttp3 = true\n{}\
                 [[l4]]\nname = \"db\"\nlisten = \"{}\"\nprotocol = \"{}\"\n[[l4.upstreams]]\naddr = \"127.0.0.1:1\"\n",
                TLS, listen, proto
            )
        };
        assert!(err(&l4("127.0.0.1:8443", "tcp")).contains("[[l4]] \"db\""));
        assert!(err(&l4("127.0.0.1:8443", "udp")).contains("same UDP address"));
        specs(&l4("127.0.0.1:3306", "tcp")).unwrap();
    }

    #[cfg(feature = "acme")]
    #[test]
    fn acme_challenge_listener_conflicts() {
        let e = err("[[listener]]\nlisten = \"0.0.0.0:80\"\ntls = false\n[acme]\nenabled = true\n");
        assert!(e.contains("[acme] challenge_listen"), "{}", e);
        // ACME が無効なら見ない
        specs("[[listener]]\nlisten = \"0.0.0.0:80\"\ntls = false\n[acme]\nenabled = false\n")
            .unwrap();
    }

    #[test]
    fn overlap_rules() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert!(socket_addrs_overlap(&a("0.0.0.0:1"), &a("10.0.0.1:1")));
        assert!(!socket_addrs_overlap(&a("0.0.0.0:1"), &a("0.0.0.0:2")));
        assert!(!socket_addrs_overlap(&a("10.0.0.1:1"), &a("10.0.0.2:1")));
        assert!(socket_addrs_overlap(&a("[::]:1"), &a("10.0.0.1:1")));
        assert!(!socket_addrs_overlap(&a("0.0.0.0:1"), &a("[::1]:1")));
    }

    #[test]
    fn describe_line() {
        let s = specs(&format!("{}{}", ARENA, TLS)).unwrap();
        let https = s.iter().find(|x| x.name == "https").unwrap();
        assert_eq!(
            https.describe(4),
            "listener \"https\" 0.0.0.0:8443 tls alpn=h2,http/1.1 http3=0.0.0.0:8443 workers=4"
        );
    }
}
