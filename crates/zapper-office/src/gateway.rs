//! The dashboard's checks on a browser's requests: a `Host` on loopback
//! (defeating DNS rebinding), and, for a write, the UI's marker header and an
//! `Origin` naming the office; the token cookie.

use hyper::{header, HeaderMap, Method, StatusCode};

/// The host names a loopback listener answers to on `port`.
pub fn local_hosts(port: u16) -> [String; 3] {
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
}

/// Whether a request's `Host` may reach the UI: a local name on loopback,
/// which defeats DNS rebinding; listening elsewhere, any well-formed host.
pub fn host_ok(host: &str, port: u16, remote: bool) -> bool {
    if remote {
        !host.is_empty()
            && host.len() <= 300
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
    } else {
        local_hosts(port).iter().any(|l| l == host)
    }
}

/// Whether a write's `Origin` is the UI itself.
pub fn origin_ok(origin: &str, host: &str, port: u16, remote: bool) -> bool {
    if remote {
        origin == format!("http://{host}") || origin == format!("https://{host}")
    } else {
        local_hosts(port)
            .iter()
            .any(|l| origin == format!("http://{l}"))
    }
}

/// A browser's request reaches a UI only by a name it may have
/// (`host_ok`); one that changes something only with the `marker` header,
/// which another site's page can't send without a preflight that is never
/// granted, and an `Origin` naming the UI (`origin_ok`). The caller maps
/// the `(status, message)` to its own error type.
pub fn from_the_ui(
    method: &Method,
    headers: &HeaderMap,
    port: u16,
    remote: bool,
    marker: &str,
) -> Result<(), (StatusCode, String)> {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !host_ok(host, port, remote) {
        return Err((StatusCode::FORBIDDEN, "bad Host header".into()));
    }
    if !matches!(*method, Method::GET | Method::HEAD) {
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let marked = headers.get(marker).is_some_and(|v| v == "1");
        if !marked || !origin_ok(origin, host, port, remote) {
            return Err((
                StatusCode::FORBIDDEN,
                format!("missing {marker} or the UI's Origin"),
            ));
        }
    }
    Ok(())
}

/// The `Set-Cookie` value that grants the browser `token` at sign-in.
pub fn set_cookie(name: &str, token: &str) -> String {
    format!("{name}={token}; HttpOnly; SameSite=Strict; Path=/")
}

/// The token in the browser's `cookie` header named `name`, if any.
pub fn cookie_token(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|c| c.to_str().ok())
        .flat_map(|c| c.split(';'))
        .find_map(|c| {
            c.trim()
                .strip_prefix(&format!("{name}="))
                .map(str::to_string)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_and_origin() {
        assert!(host_ok("127.0.0.1:7880", 7880, false));
        assert!(host_ok("localhost:7880", 7880, false));
        assert!(!host_ok("evil.example:7880", 7880, false), "DNS rebinding");
        assert!(host_ok("anything.example", 7880, true), "off-loopback");
        assert!(origin_ok(
            "http://127.0.0.1:7880",
            "127.0.0.1:7880",
            7880,
            false
        ));
        assert!(!origin_ok(
            "http://evil.example",
            "127.0.0.1:7880",
            7880,
            false
        ));
    }
}
