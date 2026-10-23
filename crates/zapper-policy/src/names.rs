//! Host names from the guest: DNS questions, `CONNECT` targets, `Host`
//! headers and TLS server names. All of it is untrusted input, validated here
//! before any rule sees it, so the name a rule matches is the name that was
//! asked for.

use std::net::IpAddr;

/// Why a name was rejected.
pub type Invalid = &'static str;

/// Validates one DNS label: 1–63 ASCII letters, digits, `-` or `_`, not
/// starting or ending with `-`. Internationalized names arrive in their
/// `xn--` form.
pub fn label(l: &[u8]) -> Result<(), Invalid> {
    if l.is_empty() {
        return Err("empty label");
    }
    if l.len() > 63 {
        return Err("label longer than 63 bytes");
    }
    if !l
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
    {
        return Err("label with a character other than letters, digits, '-' and '_'");
    }
    if l.first() == Some(&b'-') || l.last() == Some(&b'-') {
        return Err("label starting or ending with '-'");
    }
    Ok(())
}

/// Validates and normalises a host name: lowercased, without a trailing dot.
pub fn hostname(raw: &str) -> Result<String, Invalid> {
    let name = raw.strip_suffix('.').unwrap_or(raw);
    if name.is_empty() {
        return Err("empty name");
    }
    if name.len() > 253 {
        return Err("name longer than 253 bytes");
    }
    for l in name.split('.') {
        label(l.as_bytes())?;
    }
    Ok(name.to_ascii_lowercase())
}

/// A host in a `CONNECT` target or `Host` header: an IP address (IPv6 without
/// brackets) or a host name.
pub fn host(raw: &str) -> Result<String, Invalid> {
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Ok(ip.to_string());
    }
    hostname(raw)
}

/// Whether `name` can name a worker: lowercase letters, digits and dashes.
pub fn valid_worker_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames() {
        assert_eq!(hostname("API.GitHub.com."), Ok("api.github.com".into()));
        assert_eq!(
            hostname("_dmarc.example.com"),
            Ok("_dmarc.example.com".into())
        );
        assert_eq!(
            hostname("xn--bcher-kva.example"),
            Ok("xn--bcher-kva.example".into())
        );
        for bad in [
            "",
            ".",
            "a..b",
            "-a.com",
            "a-.com",
            "a b.com",
            "a\0.com",
            "ü.com",
            "a/b",
            "a%2e.com",
            "*.com",
            &"a".repeat(64),
            &format!("{}.com", "a.".repeat(130)),
        ] {
            assert!(hostname(bad).is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(host("10.0.0.1"), Ok("10.0.0.1".into()));
        assert_eq!(host("::1"), Ok("::1".into()));
        assert!(host("[::1]").is_err());
    }
}
