//! Secrets minted by a host command (`command:` in a secret): what to run,
//! and what it prints, `{"value": "...", "expires_at": "<RFC 3339>"}` or a
//! bare value that lives for `ttl_secs`. The daemon runs it and keeps the
//! result (`zapper_core::mint`). The output is the secret: it is parsed here
//! and never put in an error.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};

/// A mint timeout, unless the secret sets one.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a bare value (one with no `expires_at`) is kept, unless the
/// secret sets `ttl_secs`.
const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// What to run and how long to wait for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub argv: Vec<String>,
    pub timeout: Duration,
    /// The life of a bare value.
    pub ttl: Duration,
}

impl Spec {
    pub fn new(argv: &[String], timeout_secs: Option<u64>, ttl_secs: Option<u64>) -> Spec {
        Spec {
            argv: argv.to_vec(),
            timeout: timeout_secs.map_or(DEFAULT_TIMEOUT, Duration::from_secs),
            ttl: ttl_secs.map_or(DEFAULT_TTL, Duration::from_secs),
        }
    }

    /// What identifies a secret in the cache: its command line.
    pub fn key(&self) -> String {
        self.argv.join("\0")
    }
}

/// A freshly minted value and when it stops working (unix seconds).
#[derive(Clone)]
pub struct Minted {
    pub value: String,
    pub expires: u64,
}

impl std::fmt::Debug for Minted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Minted(expires {})", self.expires)
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn rfc3339(t: &str, field: &str) -> Result<u64> {
    Ok(
        time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339)
            .map_err(|_| anyhow!("{field} is not an RFC 3339 time"))?
            .unix_timestamp()
            .max(0) as u64,
    )
}

/// `{"value", "expires_at"}`, AWS credential_process JSON (`AccessKeyId`,
/// `SecretAccessKey`, `SessionToken`?, `Expiration`?: the value is that
/// JSON, for an `aws` secret), or a bare value that lives for `ttl`.
pub fn parse_output(text: &str, ttl: Duration, now: u64) -> Result<Minted> {
    let text = text.trim();
    if text.is_empty() {
        bail!("the command printed nothing");
    }
    if text.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(text)
            .map_err(|_| anyhow!("the command's output is not valid JSON"))?;
        if v.get("AccessKeyId").is_some() {
            let field = |k: &str| v[k].as_str().filter(|s| !s.is_empty());
            let (Some(id), Some(key)) = (field("AccessKeyId"), field("SecretAccessKey")) else {
                bail!("the command's AWS credentials need AccessKeyId and SecretAccessKey");
            };
            let mut creds = serde_json::json!({ "AccessKeyId": id, "SecretAccessKey": key });
            if let Some(t) = field("SessionToken") {
                creds["SessionToken"] = t.into();
            }
            let expires = match field("Expiration") {
                Some(t) => rfc3339(t, "Expiration")?,
                None => now + ttl.as_secs(),
            };
            return Ok(Minted {
                value: creds.to_string(),
                expires,
            });
        }
        let value = v["value"].as_str().filter(|s| !s.is_empty());
        let value = value.ok_or_else(|| anyhow!("the command's JSON has no \"value\""))?;
        let expires = match v["expires_at"].as_str() {
            Some(t) => rfc3339(t, "expires_at")?,
            None => now + ttl.as_secs(),
        };
        return Ok(Minted {
            value: value.into(),
            expires,
        });
    }
    if text.contains('\n') {
        bail!("the command printed several lines; a bare value is one line");
    }
    Ok(Minted {
        value: text.into(),
        expires: now + ttl.as_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_credential_process_output_is_a_value_that_expires() {
        let m = parse_output(
            r#"{"Version":1,"AccessKeyId":"ASIA1","SecretAccessKey":"sk","SessionToken":"tok","Expiration":"2030-01-01T00:00:00Z"}"#,
            DEFAULT_TTL,
            5,
        )
        .unwrap();
        assert_eq!(m.expires, 1_893_456_000);
        let v: serde_json::Value = serde_json::from_str(&m.value).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"AccessKeyId":"ASIA1","SecretAccessKey":"sk","SessionToken":"tok"})
        );
        // Long-lived keys live for the ttl; a missing half is an error that
        // doesn't show the other.
        let m = parse_output(
            r#"{"AccessKeyId":"AKIA1","SecretAccessKey":"sk"}"#,
            Duration::from_secs(60),
            100,
        )
        .unwrap();
        assert_eq!(m.expires, 160);
        let e = parse_output(
            r#"{"AccessKeyId":"AKIA1","SessionToken":"topsecret"}"#,
            DEFAULT_TTL,
            0,
        )
        .unwrap_err();
        assert!(!format!("{e:#}").contains("topsecret"));
    }

    #[test]
    fn output_is_json_or_a_bare_value() {
        let m = parse_output(
            r#"{"value":"ghs_x","expires_at":"2030-01-01T00:00:00Z"}"#,
            DEFAULT_TTL,
            5,
        )
        .unwrap();
        assert_eq!((m.value.as_str(), m.expires), ("ghs_x", 1_893_456_000));
        let m = parse_output(" plain \n", Duration::from_secs(60), 100).unwrap();
        assert_eq!((m.value.as_str(), m.expires), ("plain", 160));
        let m = parse_output(r#"{"value":"v"}"#, Duration::from_secs(60), 100).unwrap();
        assert_eq!(m.expires, 160);
        for bad in [
            "",
            "  ",
            "a\nb",
            "{",
            r#"{"value":""}"#,
            r#"{"expires_at":"2030-01-01T00:00:00Z"}"#,
        ] {
            assert!(parse_output(bad, DEFAULT_TTL, 0).is_err(), "{bad:?}");
        }
        let e = parse_output(
            r#"{"value":"topsecret","expires_at":"soon"}"#,
            DEFAULT_TTL,
            0,
        )
        .unwrap_err();
        assert!(!format!("{e:#}").contains("topsecret"));
        let e = parse_output("topsecret\nsecond", DEFAULT_TTL, 0).unwrap_err();
        assert!(!format!("{e:#}").contains("topsecret"));
    }
}
