//! BLADE_PROXY parsing: validated, redacted proxy descriptors.
//!
//! The raw value may embed credentials (`http://user:pass@host:port`). When
//! valid it is still handed to Chrome as-is — but it must NEVER reach a log
//! line, error message or diagnostic. Every read site goes through
//! [`env_blade_proxy`], which validates the shape and returns a redacted
//! display string alongside the raw server value; validation errors describe
//! the problem without echoing any part of the value.

/// What Chrome receives vs what logs may show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySpec {
    /// The value handed to Chrome's `--proxy-server`. May contain
    /// credentials — never log this field.
    pub server: String,
    /// Redacted endpoint for logs/diagnostics: scheme/host/port only, any
    /// credentials replaced by a fixed marker. Never contains the raw value.
    pub display: String,
    /// True when the raw value carried a userinfo section (credentials).
    pub has_credentials: bool,
}

/// `BLADE_PROXY` from the environment, parsed and validated. `Ok(None)` when
/// unset/empty (no proxy); `Err(reason)` for a malformed value — callers fail
/// the launch with the reason, never with the value.
pub fn env_blade_proxy() -> Result<Option<ProxySpec>, String> {
    match std::env::var("BLADE_PROXY") {
        Ok(v) if !v.is_empty() => parse_blade_proxy(&v).map(Some),
        _ => Ok(None),
    }
}

/// Validate a proxy value and derive its redacted display form.
///
/// Accepted shapes (what Chrome's `--proxy-server` documents):
/// `scheme://host:port` with scheme in http/https/socks4/socks5/quic, or a
/// bare `host:port`. Rejected loudly: other schemes, whitespace/control
/// bytes, empty host/port, non-numeric or out-of-range ports, path/query
/// characters. Reasons never include any byte of the input — a malformed
/// value could hide a secret in any position.
pub fn parse_blade_proxy(raw: &str) -> Result<ProxySpec, String> {
    let v = raw.trim();
    if v.is_empty() {
        return Err("empty value".to_string());
    }
    if v.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("contains whitespace or control characters".to_string());
    }
    let (scheme, had_scheme, rest) = match v.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), true, r.to_string()),
        None => ("http".to_string(), false, v.to_string()),
    };
    if !matches!(
        scheme.as_str(),
        "http" | "https" | "socks4" | "socks5" | "quic"
    ) {
        return Err(
            "unsupported scheme (expected http, https, socks4, socks5 or quic)".to_string(),
        );
    }
    // Split off userinfo at the LAST '@' — a password may itself contain one.
    let (userinfo, hostport) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_string()), h.to_string()),
        None => (None, rest),
    };
    if let Some(u) = &userinfo {
        if u.is_empty() {
            return Err("empty credentials section".to_string());
        }
    }
    // host[:port]; a bracketed [v6] literal may contain colons of its own.
    let (host, port) = if let Some(hp) = hostport.strip_prefix('[') {
        match hp.split_once(']') {
            Some((h, after)) => {
                if h.is_empty() {
                    return Err("empty IPv6 host".to_string());
                }
                match after {
                    "" => (format!("[{h}]"), None),
                    a if a.starts_with(':') => (format!("[{h}]"), Some(a[1..].to_string())),
                    _ => return Err("unexpected characters after IPv6 literal".to_string()),
                }
            }
            None => return Err("unterminated IPv6 literal".to_string()),
        }
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => {
                if p.is_empty() {
                    return Err("empty port after ':'".to_string());
                }
                (h.to_string(), Some(p.to_string()))
            }
            None => (hostport, None),
        }
    };
    if host.is_empty() {
        return Err("empty host".to_string());
    }
    if !host.starts_with('[')
        && !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err("host contains unexpected characters".to_string());
    }
    let port = match port {
        Some(p) => {
            let n: u16 = p
                .parse()
                .map_err(|_| "port is not a number in 1-65535".to_string())?;
            if n == 0 {
                return Err("port 0 is not a valid proxy port".to_string());
            }
            Some(n)
        }
        None => None,
    };
    let endpoint = match port {
        Some(p) => format!("{host}:{p}"),
        None => host.clone(),
    };
    let display = {
        let mut d = if had_scheme {
            format!("{scheme}://{endpoint}")
        } else {
            endpoint.clone()
        };
        if userinfo.is_some() {
            d.push_str(" (credentials redacted)");
        }
        d
    };
    Ok(ProxySpec {
        server: v.to_string(),
        display,
        has_credentials: userinfo.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_forms_parse_and_display_without_credentials() {
        let s = parse_blade_proxy("http://127.0.0.1:8888").unwrap();
        assert_eq!(s.server, "http://127.0.0.1:8888");
        assert_eq!(s.display, "http://127.0.0.1:8888");
        assert!(!s.has_credentials);

        let s = parse_blade_proxy("socks5://proxy.example.com:1080").unwrap();
        assert_eq!(s.display, "socks5://proxy.example.com:1080");

        // Bare host:port (Chrome accepts it; display stays as given).
        let s = parse_blade_proxy("10.0.0.7:3128").unwrap();
        assert_eq!(s.display, "10.0.0.7:3128");

        // IPv6 literal.
        let s = parse_blade_proxy("http://[::1]:8080").unwrap();
        assert_eq!(s.display, "http://[::1]:8080");

        // Host without an explicit port stays accepted (scheme default).
        let s = parse_blade_proxy("http://proxy.internal").unwrap();
        assert_eq!(s.display, "http://proxy.internal");
    }

    #[test]
    fn credentials_are_redacted_from_display_but_server_keeps_raw() {
        let s = parse_blade_proxy("http://user:SEKRITPW@proxy.example.com:8888").unwrap();
        assert_eq!(s.server, "http://user:SEKRITPW@proxy.example.com:8888");
        assert!(!s.display.contains("SEKRITPW"));
        assert!(!s.display.contains("user"));
        assert_eq!(
            s.display,
            "http://proxy.example.com:8888 (credentials redacted)"
        );
        assert!(s.has_credentials);

        // '@' inside the password: split at the LAST '@'.
        let s = parse_blade_proxy("socks5://u:p@ss@127.0.0.1:1080").unwrap();
        assert!(!s.display.contains("p@ss"));
        assert_eq!(s.display, "socks5://127.0.0.1:1080 (credentials redacted)");
    }

    #[test]
    fn malformed_values_reject_without_echoing_any_input_bytes() {
        let secrets = [
            "ftp://user:SEKRITPW@host:21",     // unsupported scheme
            "http://user:SEKRITPW@:8080",      // empty host
            "http://user:SEKRITPW@host:port",  // non-numeric port
            "http://user:SEKRITPW@host:0",     // port 0
            "http://user:SEKRITPW@host:99999", // out-of-range port
            "http://user:SEKRITPW@host:",      // empty port
            "http://user:SEKRITPW@host/path",  // path characters
            "http://user:SEKRITPW@host :8080", // whitespace
            "http://SEKRITPW@",                // empty host after creds
            "SEKRITPW\nhttp://x:1",            // control characters
            "http://user:SEKRITPW@[::1",       // unterminated v6
        ];
        for raw in secrets {
            let err = parse_blade_proxy(raw).expect_err(raw);
            assert!(!err.contains("SEKRITPW"), "reason leaked secret: {err}");
            assert!(!err.contains("user"), "reason leaked userinfo: {err}");
            assert!(!err.contains("ftp"), "reason leaked scheme: {err}");
            assert!(err.len() < 120, "reason should be short: {err}");
        }
    }

    #[test]
    fn empty_value_is_the_only_silent_none() {
        assert!(parse_blade_proxy("").is_err());
        assert!(parse_blade_proxy("   ").is_err());
    }
}
