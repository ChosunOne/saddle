//! OpenAI's published standard API prices and ChatGPT plan windows (D93).

use std::time::{Duration, UNIX_EPOCH};

use serde_json::Value;

use super::{Auth, Price, ProviderConfig, Quota, QuotaWindow};

/// Published API rates, never subscription charges or a guess for a compatible server.
/// Source: https://developers.openai.com/api/docs/pricing.md, checked 2026-11-13.
/// Tiered long-context models are omitted until accounting can select their price per request.
pub fn price(config: &ProviderConfig) -> Option<Price> {
    if config.auth != Auth::ApiKey
        || config.base_url.trim_end_matches('/') != "https://api.openai.com/v1"
    {
        return None;
    }
    let model = config.model.as_str();
    let rates = [
        ("gpt-5.3-codex", 1.75, 14.0, 0.175),
        ("gpt-5.2", 1.75, 14.0, 0.175),
        ("gpt-5.1", 1.25, 10.0, 0.125),
        ("gpt-5", 1.25, 10.0, 0.125),
        ("gpt-5-mini", 0.25, 2.0, 0.025),
        ("gpt-5-nano", 0.05, 0.4, 0.005),
        ("gpt-5.4-mini", 0.75, 4.5, 0.075),
        ("gpt-5.4-nano", 0.2, 1.25, 0.02),
        ("gpt-4.1", 2.0, 8.0, 0.5),
        ("gpt-4.1-mini", 0.4, 1.6, 0.1),
        ("gpt-4o", 2.5, 10.0, 1.25),
        ("gpt-4o-mini", 0.15, 0.6, 0.075),
        ("o3", 2.0, 8.0, 0.5),
    ];
    let (_, input, output, cached) = rates.iter().find(|(name, ..)| {
        model == *name
            || model.strip_prefix(name).is_some_and(|suffix| {
                let bytes = suffix.as_bytes();
                bytes.len() == 11
                    && bytes[0] == b'-'
                    && bytes[5] == b'-'
                    && bytes[8] == b'-'
                    && bytes
                        .iter()
                        .enumerate()
                        .all(|(i, b)| matches!(i, 0 | 5 | 8) || b.is_ascii_digit())
            })
    })?;
    Some(Price {
        input: input / 1e6,
        output: output / 1e6,
        cache_read: Some(cached / 1e6),
        cache_write: None,
    })
}

fn stamp(seconds: u64) -> Option<String> {
    let at = UNIX_EPOCH.checked_add(Duration::from_secs(seconds))?;
    // Avoid a timestamp outside the range the display's RFC3339 parser accepts.
    (seconds < 253_402_300_800).then(|| humantime::format_rfc3339_seconds(at).to_string())
}

/// The WHAM usage response: used_percent is not the remaining allowance.
pub fn quota(page: &Value) -> Option<Quota> {
    let limits = page.get("rate_limit")?;
    let mut windows = Vec::new();
    for field in ["primary_window", "secondary_window"] {
        let window = &limits[field];
        let Some(used) = window["used_percent"]
            .as_f64()
            .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
        else {
            continue;
        };
        let Some(seconds) = window["limit_window_seconds"].as_u64().filter(|n| *n > 0) else {
            continue;
        };
        let label = match seconds {
            18_000 => "5h".into(),
            604_800 => "weekly".into(),
            n if n % 3600 == 0 => format!("{}h", n / 3600),
            n => format!("{n}s"),
        };
        windows.push(QuotaWindow {
            label,
            percent: 100.0 - used,
            reset: window["reset_at"].as_u64().and_then(stamp),
        });
    }
    Some(Quota {
        windows,
        ..Quota::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Provider, ProviderEntry, Reported, Tokens, endpoint};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn usage_get_uses_oauth_and_account_headers_not_the_api_key() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/usage", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line.to_ascii_lowercase());
            }
            assert!(head.starts_with("get /usage "));
            assert!(head.contains("authorization: bearer oauth-access"));
            assert!(head.contains("chatgpt-account-id: account-123"));
            assert!(!head.contains("wrong-api-key"));
            stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 17\r\nconnection: close\r\n\r\n{\"rate_limit\":{}}").unwrap();
        });
        let config = endpoint("openai-chatgpt", &BTreeMap::new()).unwrap();
        assert_eq!(
            config.usage_url.as_deref(),
            Some("https://chatgpt.com/backend-api/wham/usage")
        );
        let flow = config.oauth.clone().unwrap();
        let mut p = Provider::new(
            "openai-chatgpt",
            config,
            Some("wrong-api-key"),
            &Reported::default(),
        );
        p.with_oauth(
            flow,
            Tokens {
                access: "oauth-access".into(),
                refresh: "unused".into(),
                expires_at: 0,
                client_id: None,
                account_id: Some("account-123".into()),
            },
            "unused-auth-store".into(),
        );
        assert_eq!(p.usage(&url).unwrap(), json!({"rate_limit": {}}));
        server.join().unwrap();
    }

    #[test]
    fn plan_windows_keep_independent_resets_and_zero_remaining() {
        let q = quota(&json!({"rate_limit": {
            "primary_window": {"used_percent": 100, "limit_window_seconds": 18000, "reset_at": 1800000000},
            "secondary_window": {"used_percent": 42, "limit_window_seconds": 604800, "reset_at": 1800600000}
        }})).unwrap();
        assert_eq!(q.windows[0].label, "5h");
        assert_eq!(q.windows[0].percent, 0.0);
        assert_eq!(q.windows[1].label, "weekly");
        assert_eq!(q.windows[1].percent, 58.0);
        assert_ne!(q.windows[0].reset, q.windows[1].reset);
        assert!(!q.is_empty());
        assert!(quota(&json!({"rate_limit": {"primary_window": {"used_percent": -1, "limit_window_seconds": 18000}}})).unwrap().is_empty());
    }

    #[test]
    fn rates_do_not_leak_to_subscriptions_compatible_hosts_or_unknown_variants() {
        let mut c = endpoint("openai", &BTreeMap::<String, ProviderEntry>::new()).unwrap();
        c.model = "gpt-5.3-codex".into();
        assert_eq!(price(&c).unwrap().input, 1.75e-6);
        c.model = "gpt-5-mini-2025-08-07".into();
        assert_eq!(price(&c).unwrap().input, 0.25e-6);
        c.model = "gpt-5-unknown".into();
        assert!(price(&c).is_none());
        c.model = "gpt-5".into();
        c.auth = Auth::Oauth;
        assert!(price(&c).is_none());
        c.auth = Auth::ApiKey;
        c.base_url = "https://compatible.test/v1".into();
        assert!(price(&c).is_none());
    }
}
