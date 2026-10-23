// SPDX-License-Identifier: AGPL-3.0-only

//! The one rule for a link tocsin fetches or shows: the configuration's
//! check targets and page links, and a release's link, all use it. A
//! path on the page's own site, such as the logo's, has its own rule.

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

/// A path on this site: starts with one `/`, with no backslash,
/// whitespace or control characters, so a browser cannot read it as
/// another host.
///
/// # Errors
///
/// The message to report, naming the path.
pub fn site_path(path: &str) -> Result<(), String> {
    let rooted = path.starts_with('/') && !path.starts_with("//");
    let clean = !path.contains(|c: char| c == '\\' || c.is_whitespace() || c.is_control());
    if rooted && clean {
        Ok(())
    } else {
        Err(format!(
            "`{path}` is not a path on this site, such as \"/brand/logo.svg\""
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{http_url, site_path};

    #[test]
    fn a_site_path_cannot_name_another_host() {
        for good in ["/logo.svg", "/brand/logo.png?v=2"] {
            assert!(site_path(good).is_ok(), "{good}");
        }
        for bad in [
            "logo.svg",
            "//cdn.example.com/logo.svg",
            "/\\cdn.example.com/logo.svg",
            "https://example.com/logo.svg",
            "/a b.svg",
            "",
        ] {
            assert!(site_path(bad).is_err(), "{bad}");
        }
    }

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
