//! What the proxy adds to an inspected request and how it shapes it upstream:
//! secret placeholders rendered into headers (only where a secret may go),
//! hop-by-hop headers stripped, the Host header pinned to the checked host,
//! and the upstream request URI built so a path can never smuggle an authority.

use hyper::header::{HeaderName, HeaderValue, HOST};
use zapper_policy::{placeholder, render, NetEffects};

use crate::daemon::Daemon;

const HOP_HEADERS: [&str; 7] = [
    "connection",
    "proxy-connection",
    "proxy-authorization",
    "proxy-authenticate",
    "keep-alive",
    "upgrade",
    "te",
];

/// Secrets added to a request: their names, and their values, which the
/// request log replaces with `***` wherever they show up.
pub(crate) struct Injected {
    pub(crate) names: Vec<String>,
    pub(crate) values: Vec<String>,
}

/// Replaces secret placeholders in headers and applies `set_headers`, for
/// the secrets this host may receive. Returns the secrets used.
pub(super) fn inject(
    d: &Daemon,
    host: &str,
    plaintext: bool,
    effects: &NetEffects,
    headers: &mut hyper::HeaderMap,
) -> Result<Injected, String> {
    inject_with(&d.config.read().unwrap(), host, plaintext, effects, headers)
}

/// `inject` with the config given.
pub(crate) fn inject_with(
    config: &zapper_policy::Config,
    host: &str,
    plaintext: bool,
    effects: &NetEffects,
    headers: &mut hyper::HeaderMap,
) -> Result<Injected, String> {
    let mut used = Vec::new();
    let mut values = Vec::new();
    let value_of = |name: &str| -> Result<String, String> {
        let secret = config
            .secrets
            .get(name)
            .ok_or_else(|| format!("unknown secret {name}"))?;
        if !secret.allows_host(host) {
            return Err(format!("secret {name} may not be sent to {host}"));
        }
        if plaintext && !secret.plaintext {
            return Err(format!("secret {name} may not be sent over plain HTTP (set plaintext: true on it to allow)"));
        }
        secret
            .source
            .resolve()
            .map_err(|e| format!("secret {name}: {e:#}"))
    };
    for name in &effects.secrets {
        let ph = placeholder(name);
        let present = headers
            .values()
            .any(|v| v.to_str().is_ok_and(|s| s.contains(&ph)));
        if !present {
            continue;
        }
        let value = value_of(name)?;
        values.push(value.clone());
        for v in headers.values_mut() {
            if let Ok(s) = v.to_str() {
                if s.contains(&ph) {
                    *v = HeaderValue::from_str(&s.replace(&ph, &value))
                        .map_err(|_| format!("secret {name} is not a valid header value"))?;
                }
            }
        }
        used.push(name.clone());
    }
    for (k, template) in &effects.set_headers {
        let mut err = None;
        let value = render(template, |name| match value_of(name) {
            Ok(v) => {
                used.push(name.to_string());
                values.push(v.clone());
                v
            }
            Err(e) => {
                err = Some(e);
                String::new()
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        let name =
            HeaderName::from_bytes(k.as_bytes()).map_err(|_| format!("bad header name {k}"))?;
        headers.insert(
            name,
            HeaderValue::from_str(&value).map_err(|_| format!("bad value for header {k}"))?,
        );
    }
    used.sort();
    used.dedup();
    Ok(Injected {
        names: used,
        values,
    })
}

pub(crate) fn clean_headers(
    headers: &mut hyper::HeaderMap,
    host: &str,
    port: u16,
    default_port: u16,
) {
    for h in HOP_HEADERS {
        headers.remove(h);
    }
    // The Host header is the checked host, whatever the client sent.
    let host_header = if port == default_port {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    headers.insert(
        HOST,
        HeaderValue::from_str(&host_header).unwrap_or(HeaderValue::from_static("invalid")),
    );
}

/// The request URI to send upstream. HTTP/1 keeps the origin-form path the
/// guest sent; HTTP/2 needs an absolute URI so hyper can set `:authority` and
/// `:scheme`. `host` is the already-validated checked host and `path` the
/// already-parsed path-and-query, so neither can smuggle an authority.
pub(crate) fn upstream_uri(host: &str, port: u16, path: &str, h2: bool) -> hyper::Uri {
    if !h2 {
        return path.parse().unwrap_or_default();
    }
    // An IPv6 literal must be bracketed in an authority.
    let h = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let authority = if port == 443 {
        h
    } else {
        format!("{h}:{port}")
    };
    // Assemble through the builder so the authority is set independently of
    // the path: a path (even a malformed one) can never bleed into it.
    hyper::Uri::builder()
        .scheme("https")
        .authority(authority)
        .path_and_query(if path.is_empty() { "/" } else { path })
        .build()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestCore;
    use zapper_policy::{placeholder, NetEffects};

    /// What goes upstream names the host that was checked, whatever Host
    /// the client sent, and carries no hop-by-hop headers.
    #[test]
    fn the_host_header_is_the_checked_host() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("host", "evil.example".parse().unwrap());
        headers.insert("proxy-authorization", "x".parse().unwrap());
        headers.insert("connection", "keep-alive".parse().unwrap());
        clean_headers(&mut headers, "api.example", 443, 443);
        assert_eq!(headers["host"], "api.example");
        assert!(headers.get("proxy-authorization").is_none());
        assert!(headers.get("connection").is_none());
        assert_eq!(headers.get_all("host").iter().count(), 1);
        // Another port is named.
        clean_headers(&mut headers, "api.example", 8443, 443);
        assert_eq!(headers["host"], "api.example:8443");
    }

    /// A secret goes only to its own `hosts`, and over plain HTTP only with
    /// `plaintext: true`, whichever rule asked for it.
    #[test]
    fn a_secret_goes_only_where_it_may() {
        let t = TestCore::new(
            "secrets:\n  s: { value: the-value, hosts: [api.example] }\n  lab: { value: lab-value, hosts: [lab.example], plaintext: true }\n",
        );
        let d = t.core.daemon();
        let add = |name: &str, host: &str, plaintext: bool| {
            let mut headers = hyper::HeaderMap::new();
            headers.insert(
                "authorization",
                format!("Bearer {}", placeholder(name)).parse().unwrap(),
            );
            let effects = NetEffects {
                secrets: vec![name.to_string()],
                set_headers: vec![],
            };
            inject(d, host, plaintext, &effects, &mut headers).map(|i| (i.names, headers))
        };
        let (names, headers) = add("s", "api.example", false).unwrap();
        assert_eq!(names, ["s"]);
        assert_eq!(headers["authorization"], "Bearer the-value");
        // A rule matching another host asks for it in vain.
        let e = add("s", "evil.example", false).unwrap_err();
        assert!(e.contains("may not be sent to evil.example"), "{e}");
        // Plain HTTP, to its own host: refused without plaintext.
        let e = add("s", "api.example", true).unwrap_err();
        assert!(e.contains("plain HTTP"), "{e}");
        let (_, headers) = add("lab", "lab.example", true).unwrap();
        assert_eq!(headers["authorization"], "Bearer lab-value");
        // set_headers is held to the same.
        let mut headers = hyper::HeaderMap::new();
        let effects = NetEffects {
            secrets: vec![],
            set_headers: vec![("x-key".into(), "{{secret:s}}".into())],
        };
        let e = inject(d, "evil.example", false, &effects, &mut headers)
            .err()
            .unwrap();
        assert!(e.contains("may not be sent to evil.example"), "{e}");
        assert!(headers.get("x-key").is_none());
    }

    /// Upstream URIs: HTTP/1 keeps the origin-form path; HTTP/2 is absolute,
    /// and the authority is always the checked host — a path cannot smuggle
    /// one in, because the scheme and authority are prefixed before it.
    #[test]
    fn upstream_uri_shape() {
        assert_eq!(
            upstream_uri("api.example", 443, "/a/b?q=1", false).to_string(),
            "/a/b?q=1"
        );
        assert_eq!(
            upstream_uri("api.example", 443, "/a/b?q=1", true).to_string(),
            "https://api.example/a/b?q=1"
        );
        // A non-default port is named in the authority.
        assert_eq!(
            upstream_uri("api.example", 8443, "/x", true).to_string(),
            "https://api.example:8443/x"
        );
        // Even a path that looks like it carries an authority stays the path.
        let u = upstream_uri("api.example", 443, "//evil.example/x", true);
        assert_eq!(u.authority().unwrap().host(), "api.example");
        assert_eq!(u.path(), "//evil.example/x");
        // An IPv6 literal is bracketed in the authority.
        assert_eq!(
            upstream_uri("::1", 8443, "/x", true).to_string(),
            "https://[::1]:8443/x"
        );
    }
}
