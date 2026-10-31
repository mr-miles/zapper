/// Placeholder the sandbox sees in place of a secret's value.
pub fn placeholder(name: &str) -> String {
    format!("zapper-secret-{name}")
}

/// The fake AWS access key id an `aws` secret's guest signs with: 20
/// characters, `ZAPPER` and fourteen derived from the name, so the proxy
/// knows which secret a signature asks for.
pub fn aws_key_id(name: &str) -> String {
    // FNV-1a, 64 bits, in base 36: stable, and enough to tell names apart
    // (collisions are refused by validation).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut digits = Vec::new();
    for _ in 0..14 {
        digits.push(b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"[(h % 36) as usize]);
        h /= 36;
    }
    format!("ZAPPER{}", String::from_utf8(digits).unwrap())
}

/// Whether an access key id is one of zapper's fakes.
pub fn is_aws_fake_key_id(id: &str) -> bool {
    id.len() == 20 && id.starts_with("ZAPPER")
}

/// How a template names a secret: `{{secret:NAME}}`.
const OPEN: &str = "{{secret:";
const CLOSE: &str = "}}";

/// Names referenced as `{{secret:NAME}}` in a template.
pub fn secret_refs(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    render(template, |name| {
        out.push(name.to_string());
        String::new()
    });
    out
}

/// Replaces `{{secret:NAME}}` with `value(NAME)`.
pub fn render(template: &str, mut value: impl FnMut(&str) -> String) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find(CLOSE) else { break };
        out.push_str(&rest[..start]);
        out.push_str(&value(&after[..end]));
        rest = &after[end + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_placeholders() {
        let t = "Bearer {{secret:gh}} and {{secret:x}}";
        assert_eq!(secret_refs(t), ["gh", "x"]);
        assert_eq!(
            render(t, placeholder),
            "Bearer zapper-secret-gh and zapper-secret-x"
        );
        assert_eq!(render("plain {{secret:", placeholder), "plain {{secret:");
    }
}
