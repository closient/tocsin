// SPDX-License-Identifier: AGPL-3.0-only

//! The one rule for a link tocsin fetches or shows: the configuration's
//! check targets and page links, and a release's link, all use it.

/// An `http://` or `https://` URL with a host and no whitespace or
/// control characters.
///
/// # Errors
///
/// The message to report, naming the URL.
pub fn http_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    let host = rest.map(|rest| rest.split(['/', '?', '#']).next().unwrap_or_default());
    let clean = !url.contains(|c: char| c.is_whitespace() || c.is_control());
    match host {
        Some(host) if !host.is_empty() && clean => Ok(()),
        _ => Err(format!("`{url}` is not an http:// or https:// URL")),
    }
}

#[cfg(test)]
mod tests {
    use super::http_url;

    #[test]
    fn only_http_and_https_with_a_host() {
        for good in [
            "https://example.com/health",
            "http://127.0.0.1:8080",
            "https://example.com?q=1",
        ] {
            assert!(http_url(good).is_ok(), "{good}");
        }
        for bad in [
            "example.com",
            "https://",
            "https:///path",
            "//example.com",
            "javascript:alert(1)",
            "JavaScript://x%0aalert(1)",
            "data:text/html,x",
            "ftp://example.com",
            "https://example.com/a b",
            "https://example.com/\u{7}",
        ] {
            assert!(http_url(bad).is_err(), "{bad}");
        }
    }
}
