// SPDX-License-Identifier: AGPL-3.0-only

//! Markdown for incident updates, rendered to HTML that is safe to put in
//! a page.
//!
//! Raw HTML in the source is shown as text, not passed through, and the
//! result is cleaned by `ammonia` anyway, so a link to `javascript:` or an
//! `onerror` attribute cannot survive either step.

use std::collections::HashSet;

use pulldown_cmark::{Event, Options, Parser};

/// `text` as sanitized HTML.
pub fn render(text: &str) -> String {
    let events = Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES).map(
        |event| match event {
            Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
            other => other,
        },
    );
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    ammonia::Builder::default()
        .url_schemes(HashSet::from(["http", "https", "mailto"]))
        .link_rel(Some("noopener noreferrer nofollow"))
        .rm_tags(["img"])
        .clean(&html)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn markdown_renders_and_html_never_runs() {
        assert_eq!(
            render("**down** for [some](https://example.com)"),
            "<p><strong>down</strong> for <a href=\"https://example.com\" rel=\"noopener noreferrer nofollow\">some</a></p>\n"
        );
        for hostile in [
            "<script>alert(1)</script>",
            "x <script>alert(1)</script> y",
            "<img src=x onerror=alert(1)>",
            "[click](javascript:alert(1))",
            "<a href=\"javascript:alert(1)\">x</a>",
            "<iframe src=\"https://example.com\"></iframe>",
        ] {
            let html = render(hostile);
            for live in ["<script", "<img", "<iframe", "<a href=\"javascript"] {
                assert!(!html.contains(live), "{hostile:?} rendered {html:?}");
            }
        }
        assert!(render("<script>alert(1)</script>").contains("&lt;script&gt;"));
    }
}
