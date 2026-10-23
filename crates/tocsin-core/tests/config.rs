// SPDX-License-Identifier: AGPL-3.0-only

//! Loading configuration: every error class, located by file, line and
//! column.

use tocsin_core::config::{
    CheckType, Config, Diagnostic, Environment, Id, NotifierType, Source, load,
};

const HOSTNAME: &str = "probe-01";

fn variable(name: &str) -> Option<String> {
    match name {
        "SLACK_WEBHOOK_URL" => {
            Some("https://hooks.slack.com/services/T0/B0/SECRET-VALUE".to_owned())
        }
        "SMTP_URL" => Some("smtp://user:hunter2@mail.example.com:587".to_owned()),
        "EMPTY" => Some(String::new()),
        _ => None,
    }
}

fn run(files: &[(&str, &str)]) -> Result<Config, Vec<Diagnostic>> {
    let sources: Vec<Source> = files
        .iter()
        .map(|(name, text)| Source {
            name: (*name).to_owned(),
            text: (*text).to_owned(),
        })
        .collect();
    let environment = Environment {
        hostname: HOSTNAME.to_owned(),
        variable: &variable,
    };
    load(&sources, &environment)
}

/// The errors as `file:line:column: message`, without hints.
fn errors(files: &[(&str, &str)]) -> Vec<String> {
    let diagnostics = run(files).expect_err("configuration should be invalid");
    diagnostics
        .iter()
        .map(|diagnostic| {
            format!(
                "{}: {}",
                diagnostic.location().unwrap_or_default(),
                diagnostic.message
            )
        })
        .collect()
}

fn only_error(files: &[(&str, &str)]) -> Diagnostic {
    let mut diagnostics = run(files).expect_err("configuration should be invalid");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    diagnostics.remove(0)
}

#[test]
fn an_empty_folder_is_a_valid_configuration() {
    let config = run(&[]).expect("valid");
    assert_eq!(config.server.listen.to_string(), "127.0.0.1:8080");
    assert_eq!(config.server.retention_days, 90);
    assert_eq!(config.server.node_id.as_deref(), Some(HOSTNAME));
    assert_eq!(config.checks, []);
}

#[test]
fn files_merge_and_references_resolve_across_them() {
    let config = run(&[
        (
            "server.toml",
            r#"
[server]
listen = "0.0.0.0:9000"
node_id = "eu-1"

[server.email]
from = "Status <status@example.com>"
reply_to = "support@example.com"
smtp_url = "${ENV:SMTP_URL}"
"#,
        ),
        (
            "components.toml",
            r#"
[[group]]
id = "core"
name = "Core"

[[component]]
id = "api"
name = "API"
group = "core"
"#,
        ),
        (
            "checks/api.toml",
            r#"
[[check]]
id = "api-health"
component = "api"
type = "http"
target = "https://api.example.com/health"
interval = "30s"
timeout = "5s"

[[check.assert]]
status = [200]

[[check.assert]]
latency_ms = { lt = 800 }
on_fail = "degraded"

[[check.assert]]
json = { path = "$.status", eq = "ok" }
"#,
        ),
        (
            "notify.toml",
            r#"
[[notifier]]
id = "ops-slack"
type = "slack"
[notifier.slack]
webhook_url = "${ENV:SLACK_WEBHOOK_URL}"

[[notifier]]
id = "ops-email"
type = "email"
[notifier.email]
to = ["ops@example.com"]

[[route]]
id = "everything"
notifiers = ["ops-slack", "ops-email"]
"#,
        ),
    ])
    .expect("valid");
    assert_eq!(config.server.node_id.as_deref(), Some("eu-1"));
    assert_eq!(config.groups.len(), 1);
    assert_eq!(
        config.components[0].group.as_ref().map(Id::as_str),
        Some("core")
    );
    let check = &config.checks[0];
    assert_eq!(check.check_type, CheckType::Http);
    assert_eq!(check.assertions.len(), 3);
    assert!(check.http.is_some(), "http settings get their defaults");
    assert_eq!(config.notifiers[0].notifier_type, NotifierType::Slack);
    let webhook = &config.notifiers[0]
        .slack
        .as_ref()
        .expect("slack")
        .webhook_url;
    assert_eq!(
        webhook.expose(),
        "https://hooks.slack.com/services/T0/B0/SECRET-VALUE"
    );

    // Secrets are redacted wherever the configuration is shown.
    let shown = serde_json::to_string(&config).expect("serializes");
    assert!(!shown.contains("SECRET-VALUE"), "{shown}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(shown.contains("${ENV:SLACK_WEBHOOK_URL}"), "{shown}");
    let debugged = format!("{config:?}");
    assert!(!debugged.contains("SECRET-VALUE") && !debugged.contains("hunter2"));
}

#[test]
fn a_syntax_error_is_located() {
    let text = "[[check]]\nid = \"a\"\ntype = = \"http\"\n";
    let diagnostic = only_error(&[("checks.toml", text)]);
    assert_eq!(diagnostic.location().as_deref(), Some("checks.toml:3:8"));
    assert!(
        diagnostic.message.starts_with("TOML syntax error"),
        "{diagnostic}"
    );
}

#[test]
fn an_unknown_field_is_located_at_its_key_with_a_suggestion() {
    let text = r#"
[[check]]
id = "web"
type = "http"
target = "https://example.com"
intervall = "30s"
"#;
    let diagnostic = only_error(&[("checks.toml", text)]);
    assert_eq!(diagnostic.location().as_deref(), Some("checks.toml:6:1"));
    assert_eq!(diagnostic.path.as_deref(), Some("check[0].intervall"));
    assert!(
        diagnostic.message.starts_with("unknown field `intervall`"),
        "{diagnostic}"
    );
    assert_eq!(diagnostic.hint.as_deref(), Some("did you mean `interval`?"));
}

#[test]
fn a_bad_duration_is_located_at_its_value() {
    let text = r#"
[[check]]
id = "web"
type = "http"
target = "https://example.com"
interval =   "5 minutes"
"#;
    let diagnostic = only_error(&[("checks.toml", text)]);
    assert_eq!(diagnostic.location().as_deref(), Some("checks.toml:6:14"));
    assert_eq!(diagnostic.path.as_deref(), Some("check[0].interval"));
}

#[test]
fn an_unknown_component_is_located_with_a_suggestion() {
    let diagnostic = only_error(&[
        (
            "components.toml",
            "[[component]]\nid = \"api\"\nname = \"API\"\n",
        ),
        (
            "checks.toml",
            "[[check]]\nid = \"web\"\ncomponent = \"apj\"\ntype = \"tcp\"\ntarget = \"db:5432\"\n",
        ),
    ]);
    assert_eq!(diagnostic.location().as_deref(), Some("checks.toml:3:13"));
    assert_eq!(diagnostic.message, "unknown component `apj`");
    assert_eq!(diagnostic.hint.as_deref(), Some("did you mean `api`?"));
}

#[test]
fn a_duplicate_id_names_both_locations() {
    let diagnostic = only_error(&[
        ("a.toml", "[[component]]\nid = \"api\"\nname = \"API\"\n"),
        (
            "b.toml",
            "\n\n[[component]]\nid  = \"api\"\nname = \"API again\"\n",
        ),
    ]);
    assert_eq!(diagnostic.location().as_deref(), Some("b.toml:4:7"));
    assert_eq!(
        diagnostic.message,
        "duplicate component id `api`; first defined at a.toml:2:6"
    );
}

#[test]
fn a_noreply_sender_is_rejected() {
    for (address, column) in [
        ("noreply@example.com", 8),
        ("Alerts <no-reply@example.com>", 8),
        ("do-not-reply@example.com", 8),
        ("No.Reply+alerts@example.com", 8),
    ] {
        let text = format!(
            "[server.email]\nfrom = \"{address}\"\nreply_to = \"support@example.com\"\nsmtp_url = \"${{ENV:SMTP_URL}}\"\n"
        );
        let diagnostic = only_error(&[("server.toml", &text)]);
        assert_eq!(
            diagnostic.location(),
            Some(format!("server.toml:2:{column}")),
            "{address}"
        );
        assert!(
            diagnostic.message.contains("no-reply address"),
            "{diagnostic}"
        );
    }
    let text =
        "[server.email]\nfrom = \"status@example.com\"\nreply_to = \"donotreply@example.com\"\n";
    let diagnostic = only_error(&[("server.toml", text)]);
    assert_eq!(diagnostic.location().as_deref(), Some("server.toml:3:12"));
}

#[test]
fn email_notifiers_need_sender_settings() {
    let notifier = "[[notifier]]\nid = \"mail\"\ntype = \"email\"\n[notifier.email]\nto = [\"ops@example.com\"]\n";
    assert_eq!(
        errors(&[("notify.toml", notifier)]),
        ["notify.toml:3:8: notifier `mail` sends email, but no file sets `[server.email]`"]
    );
    assert_eq!(
        errors(&[
            (
                "server.toml",
                "[server.email]\nfrom = \"status@example.com\"\n"
            ),
            ("notify.toml", notifier),
        ]),
        [
            "server.toml:1:1: `[server.email]` needs `reply_to` because notifier `mail` sends email",
            "server.toml:1:1: `[server.email]` needs `smtp_url` because notifier `mail` sends email",
        ]
    );
}

#[test]
fn secrets_come_only_from_the_environment_and_are_never_echoed() {
    let text = r#"
[[notifier]]
id = "a"
type = "slack"
[notifier.slack]
webhook_url = "https://hooks.slack.com/services/T0/B0/PASTED-SECRET"

[[notifier]]
id = "b"
type = "slack"
[notifier.slack]
webhook_url = "${ENV:MISSING_WEBHOOK}"

[[notifier]]
id = "c"
type = "slack"
[notifier.slack]
webhook_url = "${ENV:EMPTY}"
"#;
    let diagnostics = run(&[("notify.toml", text)]).expect_err("invalid");
    let shown: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
    assert_eq!(diagnostics.len(), 3, "{shown:#?}");
    assert_eq!(
        diagnostics[0].location().as_deref(),
        Some("notify.toml:6:15")
    );
    assert_eq!(
        diagnostics[1].location().as_deref(),
        Some("notify.toml:12:15")
    );
    assert_eq!(
        diagnostics[1].message,
        "environment variable `MISSING_WEBHOOK` is not set"
    );
    assert_eq!(
        diagnostics[2].message,
        "environment variable `EMPTY` is empty"
    );
    for line in &shown {
        assert!(!line.contains("PASTED-SECRET"), "{line}");
    }
}

#[test]
fn environment_references_are_rejected_outside_secret_fields() {
    let text = "[[component]]\nid = \"api\"\nname = \"${ENV:SLACK_WEBHOOK_URL}\"\n";
    let diagnostic = only_error(&[("components.toml", text)]);
    assert_eq!(
        diagnostic.location().as_deref(),
        Some("components.toml:3:8")
    );
    assert!(
        diagnostic.message.contains("only read in secret fields"),
        "{diagnostic}"
    );
}

#[test]
fn tables_written_the_wrong_way_get_a_hint() {
    assert_eq!(
        errors(&[(
            "a.toml",
            "[check]\nid = \"web\"\n\n[sever]\nlisten = \"0.0.0.0:80\"\n"
        )]),
        [
            "a.toml:1:1: `check` must be an array of tables: write `[[check]]`",
            "a.toml:4:2: unknown table `sever`",
        ]
    );
    let diagnostic = &run(&[("a.toml", "[sever]\n")]).expect_err("invalid")[0];
    assert_eq!(diagnostic.hint.as_deref(), Some("did you mean `server`?"));
}

#[test]
fn server_and_page_live_in_one_file() {
    assert_eq!(
        errors(&[
            ("a.toml", "[page]\ntitle = \"A\"\n"),
            ("b.toml", "\n[page]\ntitle = \"B\"\n")
        ]),
        ["b.toml:2:1: `[page]` is already defined at a.toml:1:1"]
    );
}

#[test]
fn every_problem_in_every_file_is_reported() {
    let checks = r#"
[[check]]
id = "web"
type = "http"
target = "example.com"
interval = "10s"
timeout = "10s"

[[check.assert]]
status = [200]
latency_ms = { lt = 800 }

[[check]]
id = "db"
type = "tcp"
target = "db.example.com:5432"

[[check.assert]]
status = [200]

[[check]]
id = "Bad_Id"
type = "ftp"
target = "x"
"#;
    let routes = "[[route]]\nid = \"all\"\nnotifiers = []\nchecks = [\"web\", \"dbb\"]\n";
    assert_eq!(
        errors(&[("checks.toml", checks), ("routes.toml", routes)]),
        [
            "checks.toml:5:10: `example.com` is not an http:// or https:// URL",
            "checks.toml:7:11: `timeout` (10s) must be shorter than `interval` (10s)",
            "checks.toml:9:1: an assertion holds one condition, and this one has `status`, `latency_ms`",
            "checks.toml:19:10: a `status` assertion does not apply to tcp checks",
            "checks.toml:22:6: invalid id `Bad_Id`: use lowercase letters, digits and hyphens, starting with a letter or digit",
            "routes.toml:3:13: a route needs at least one notifier",
            "routes.toml:4:18: unknown check `dbb`",
        ]
    );
}

#[test]
fn notifier_settings_must_match_the_type() {
    let text = "[[notifier]]\nid = \"a\"\ntype = \"discord\"\n[notifier.slack]\nwebhook_url = \"${ENV:SLACK_WEBHOOK_URL}\"\n";
    assert_eq!(
        errors(&[("notify.toml", text)]),
        [
            "notify.toml:3:8: a `discord` notifier needs a `[notifier.discord]` table",
            "notify.toml:4:1: `[notifier.slack]` does not apply to a `discord` notifier",
        ]
    );
}

#[test]
fn diagnostics_serialize_with_every_key() {
    let diagnostic = only_error(&[("a.toml", "[sever]\n")]);
    let json = serde_json::to_value(&diagnostic).expect("serializes");
    assert_eq!(
        json,
        serde_json::json!({
            "file": "a.toml",
            "line": 1,
            "column": 2,
            "path": "sever",
            "message": "unknown table `sever`",
            "hint": "did you mean `server`?",
        })
    );
}
