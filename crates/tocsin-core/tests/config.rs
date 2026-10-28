// SPDX-License-Identifier: AGPL-3.0-only

//! Loading configuration: every error class, located by file, line and
//! column.

use tocsin_core::config::{
    AuthMode, CheckType, Config, DARK, Diagnostic, Environment, Id, NodeIdSource, NotifierType,
    ProbeClass, Secret, Source, TokenScope, TopologyNode, load,
};
use tocsin_core::federation::NodeKey;

const HOSTNAME: &str = "probe-01";

fn variable(name: &str) -> Option<String> {
    match name {
        "SLACK_WEBHOOK_URL" => {
            Some("https://hooks.slack.com/services/T0/B0/SECRET-VALUE".to_owned())
        }
        "SMTP_URL" => Some("smtp://user:hunter2@mail.example.com:587".to_owned()),
        "EMPTY" => Some(String::new()),
        "API_AUTHORIZATION" => Some("Bearer HEADER-SECRET".to_owned()),
        "API_BODY" => Some("{\"key\":\"BODY-SECRET\"}".to_owned()),
        "BROKEN_HEADER" => Some("Bearer LINE-SECRET\nX-Injected: yes".to_owned()),
        "TOKEN_A" | "TOKEN_B" => Some("SHARED-TOKEN-SECRET".to_owned()),
        "TOKEN_C" => Some("another-token-secret".to_owned()),
        "HTTPS_URL" => Some("https://user:HTTPS-SECRET@mail.example.com".to_owned()),
        "WEBHOOK_SECRET" => Some("SIGNING-SECRET".to_owned()),
        "SLACK_CLIENT_ID" => Some("1234.5678".to_owned()),
        "SLACK_CLIENT_SECRET" => Some("CLIENT-SECRET-VALUE".to_owned()),
        "NODE_KEY_A" => Some(NodeKey::from_seed([1; 32]).expose()),
        "NODE_KEY_B" => Some(NodeKey::from_seed([2; 32]).expose()),
        "NODE_KEY_BROKEN" => Some("ed25519:NODE-KEY-SECRET".to_owned()),
        _ => None,
    }
}

fn run(files: &[(&str, &str)]) -> Result<Config, Vec<Diagnostic>> {
    let sources: Vec<Source> = files
        .iter()
        .map(|(name, text)| Source {
            name: (*name).to_owned(),
            text: Ok((*text).to_owned()),
        })
        .collect();
    run_sources(&sources)
}

fn run_sources(sources: &[Source]) -> Result<Config, Vec<Diagnostic>> {
    let environment = Environment {
        hostname: HOSTNAME.to_owned(),
        node_id: None,
        variable: &variable,
    };
    load(sources, &environment)
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
        .webhook_url
        .as_ref()
        .expect("webhook_url");
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
fn max_body_defaults_to_64_kib_and_takes_a_size() {
    let checks = r#"
[[check]]
id = "default"
type = "http"
target = "https://example.com"

[[check]]
id = "status-page"
type = "http"
target = "https://www.cloudflarestatus.com/api/v2/components.json"
[check.http]
max_body = "512KiB"

[[check]]
id = "ceiling"
type = "http"
target = "https://example.com"
[check.http]
max_body = "8MiB"
"#;
    let config = run(&[("checks.toml", checks)]).expect("valid");
    let max_body: Vec<u64> = config
        .checks
        .iter()
        .map(|check| check.http.as_ref().expect("http settings").max_body.bytes())
        .collect();
    assert_eq!(max_body, [64 * 1024, 512 * 1024, 8 * 1024 * 1024]);
}

#[test]
fn max_body_over_the_ceiling_or_not_a_size_is_rejected_where_written() {
    let check = |id: &str, max_body: &str| {
        format!(
            "[[check]]\nid = \"{id}\"\ntype = \"http\"\ntarget = \"https://example.com\"\n[check.http]\nmax_body = {max_body}\n\n"
        )
    };
    let checks = [
        check("over", "\"9MiB\""),
        check("just-over", "\"8193KiB\""),
        check("zero", "\"0B\""),
        check("words", "\"lots\""),
        check("no-unit", "\"65536\""),
        check("decimal-unit", "\"64KB\""),
        check("integer", "65536"),
    ]
    .concat();
    let size = "write a whole number followed by B, KiB or MiB, such as \"512KiB\" or \"1MiB\"";
    assert_eq!(
        errors(&[("checks.toml", &checks)]),
        [
            "checks.toml:6:12: `max_body` (9MiB) must be at most 8MiB".to_owned(),
            "checks.toml:13:12: `max_body` (8193KiB) must be at most 8MiB".to_owned(),
            "checks.toml:20:12: `max_body` must be more than 0B".to_owned(),
            format!("checks.toml:27:12: invalid size `lots`: {size}"),
            format!("checks.toml:34:12: invalid size `65536`: {size}"),
            format!("checks.toml:41:12: invalid size `64KB`: {size}"),
            "checks.toml:48:12: invalid type: integer `65536`, expected a string".to_owned(),
        ]
    );
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
fn each_project_component_is_checked_and_located() {
    let components = "[[component]]\nid = \"api\"\nname = \"API\"\n\n[[component]]\nid = \"web\"\nname = \"Web\"\n";
    let diagnostic = only_error(&[
        ("components.toml", components),
        (
            "projects.toml",
            "[[project]]\nid = \"site\"\nname = \"Site\"\ncomponents = [\"api\", \"wbe\"]\n",
        ),
    ]);
    assert_eq!(diagnostic.path.as_deref(), Some("project[0].components[1]"));
    assert_eq!(diagnostic.message, "unknown component `wbe`");
    assert_eq!(diagnostic.hint.as_deref(), Some("did you mean `web`?"));
}

#[test]
fn a_project_is_public_unless_it_says_otherwise() {
    let config = run(&[(
        "projects.toml",
        "[[project]]\nid = \"site\"\nname = \"Site\"\n\n\
         [[project]]\nid = \"ops\"\nname = \"Ops\"\npublic = false\n\n\
         [[token]]\nid = \"reader\"\nsecret = \"${ENV:TOKEN_C}\"\nscopes = [\"status:read\"]\n",
    )])
    .expect("valid");
    let public: Vec<_> = config
        .projects
        .iter()
        .map(|project| (project.id.as_str(), project.public))
        .collect();
    assert_eq!(public, [("site", true), ("ops", false)]);
    assert_eq!(config.tokens[0].scopes, [TokenScope::StatusRead]);
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
        ("no-reply-status@example.com", 8),
        ("Alerts <alerts-noreply@example.com>", 8),
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

const SLACK: &str = "[slack]\nclient_id = \"${ENV:SLACK_CLIENT_ID}\"\nclient_secret = \"${ENV:SLACK_CLIENT_SECRET}\"\n";
const ADMIN: &str = "[server]\npublic_url = \"https://status.example.com\"\n[server.auth]\nmode = \"trusted_header\"\ntrusted_proxies = [\"10.0.0.0/8\"]\n";

#[test]
fn slack_needs_an_https_page_and_someone_to_connect() {
    let config = run(&[("server.toml", ADMIN), ("slack.toml", SLACK)]).expect("valid");
    let slack = config.slack.as_ref().expect("[slack]");
    assert_eq!(slack.client_id.expose(), "1234.5678");
    assert_eq!(slack.client_secret.expose(), "CLIENT-SECRET-VALUE");
    let shown = serde_json::to_string(&config).expect("serializes");
    assert!(!shown.contains("CLIENT-SECRET-VALUE"), "{shown}");
    assert!(shown.contains("${ENV:SLACK_CLIENT_SECRET}"), "{shown}");

    assert_eq!(
        errors(&[("slack.toml", SLACK)]),
        [
            "slack.toml:1:1: `[slack]` needs `[server] public_url`: Slack sends people back to it after they approve",
            "slack.toml:1:1: `[slack]` has no one to connect a channel: it needs the admin page or `[subscribers]`",
        ]
    );
    let plain = ADMIN.replace("https://", "http://");
    assert_eq!(
        errors(&[("server.toml", &plain), ("slack.toml", SLACK)]),
        [
            "server.toml:2:14: `[slack]` needs an `https` `public_url`: Slack sends people back only to https addresses"
        ]
    );
    let long = format!("{SLACK}app_name = \"{}\"\n", "a".repeat(36));
    assert_eq!(
        errors(&[("server.toml", ADMIN), ("slack.toml", &long)]),
        [
            "slack.toml:4:12: `app_name` must be 1 to 35 characters, as Slack requires; this one is 36"
        ]
    );
    let pasted = SLACK.replace("${ENV:SLACK_CLIENT_SECRET}", "CLIENT-SECRET-PASTED");
    let diagnostic = only_error(&[("server.toml", ADMIN), ("slack.toml", &pasted)]);
    assert_eq!(diagnostic.location().as_deref(), Some("slack.toml:3:17"));
    assert!(!diagnostic.to_string().contains("CLIENT-SECRET-PASTED"));
}

#[test]
fn a_slack_notifier_names_a_connection_or_a_webhook_not_both() {
    let notifier = |settings: &str| {
        format!("[[notifier]]\nid = \"ops\"\ntype = \"slack\"\n[notifier.slack]\n{settings}")
    };
    let config = run(&[("notify.toml", &notifier("connection = \"ops-alerts\"\n"))])
        .expect("a connection needs no [slack] to be named");
    let slack = config.notifiers[0].slack.as_ref().expect("slack");
    assert_eq!(
        slack.connection.as_ref().map(Id::as_str),
        Some("ops-alerts")
    );
    assert!(slack.webhook_url.is_none());
    for settings in [
        "",
        "connection = \"ops-alerts\"\nwebhook_url = \"${ENV:SLACK_WEBHOOK_URL}\"\n",
    ] {
        assert_eq!(
            errors(&[("notify.toml", &notifier(settings))]),
            ["notify.toml:4:1: `[notifier.slack]` needs `webhook_url` or `connection`, not both"],
            "{settings}"
        );
    }
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
fn ids_in_a_file_that_failed_to_parse_are_not_reported_unknown() {
    let components =
        "[[component]]\nid = \"web\"\nname = \"Web\"\n\n[[component]]\nid = = \"db\"\n";
    let checks = "[[check]]\nid = \"web\"\ntype = \"tcp\"\ntarget = \"web\"\ncomponent = \"db\"\n";
    assert_eq!(
        errors(&[("components.toml", components), ("checks.toml", checks)]),
        [
            "components.toml:6:6: TOML syntax error: extra `=`, expected nothing",
            "checks.toml:4:10: `web` is not a valid tcp target",
        ]
    );
}

#[test]
fn an_unreadable_file_is_reported_and_its_ids_are_not_reported_unknown() {
    let sources = [
        Source {
            name: "components.toml".to_owned(),
            text: Err("the file is not UTF-8 text".to_owned()),
        },
        Source {
            name: "checks.toml".to_owned(),
            text: Ok("[[check]]\nid = \"web\"\ntype = \"tcp\"\ntarget = \"web:80\"\ncomponent = \"web\"\n".to_owned()),
        },
    ];
    let diagnostics = run_sources(&sources).expect_err("invalid");
    let shown: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
    assert_eq!(shown, ["components.toml: the file is not UTF-8 text"]);
}

#[test]
fn a_server_table_that_fails_does_not_also_claim_server_email_is_missing() {
    let server =
        "[server]\nlisten = \"nonsense\"\n\n[server.email]\nfrom = \"status@example.com\"\n";
    let notifier = "[[notifier]]\nid = \"mail\"\ntype = \"email\"\n[notifier.email]\nto = [\"ops@example.com\"]\n";
    let diagnostic = only_error(&[("server.toml", server), ("notify.toml", notifier)]);
    assert_eq!(diagnostic.location().as_deref(), Some("server.toml:2:10"));
}

#[test]
fn http_headers_and_body_can_be_secrets() {
    let text = r#"
[[check]]
id = "api"
type = "http"
target = "https://api.example.com/health"
[check.http]
method = "POST"
headers = { Authorization = "${ENV:API_AUTHORIZATION}", Accept = "application/json" }
body = "${ENV:API_BODY}"
"#;
    let config = run(&[("checks.toml", text)]).expect("valid");
    let http = config.checks[0].http.as_ref().unwrap();
    assert_eq!(
        http.headers["Authorization"].expose(),
        "Bearer HEADER-SECRET"
    );
    assert_eq!(http.headers["Accept"].expose(), "application/json");
    assert_eq!(
        http.body.as_ref().unwrap().expose(),
        "{\"key\":\"BODY-SECRET\"}"
    );
    let shown = serde_json::to_string(&config).unwrap();
    assert!(
        !shown.contains("HEADER-SECRET") && !shown.contains("BODY-SECRET"),
        "{shown}"
    );
    assert!(
        shown.contains("<redacted from ${ENV:API_AUTHORIZATION}>"),
        "{shown}"
    );
    assert!(shown.contains("\"Accept\":\"application/json\""), "{shown}");
    let debugged = format!("{config:?}");
    assert!(!debugged.contains("HEADER-SECRET") && !debugged.contains("BODY-SECRET"));

    let broken = r#"
[[check]]
id = "api"
type = "http"
target = "https://api.example.com/health"
[check.http]
headers = { Authorization = "${ENV:MISSING_AUTHORIZATION}", X-Key = "Bearer ${ENV:API_AUTHORIZATION}" }
"#;
    let diagnostics = run(&[("checks.toml", broken)]).expect_err("invalid");
    let shown: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
    // The malformed reference fails the whole check, so the missing
    // variable beside it is only reported once that is fixed.
    assert_eq!(diagnostics.len(), 1, "{shown:#?}");
    assert_eq!(
        diagnostics[0].location().as_deref(),
        Some("checks.toml:7:69")
    );
    assert!(
        diagnostics[0]
            .message
            .starts_with("a secret must be an environment reference"),
        "{shown:#?}"
    );
    assert!(!diagnostics[0].message.contains("Bearer"), "{shown:#?}");
    let fixed = broken.replace("Bearer ${ENV:API_AUTHORIZATION}", "plain");
    let diagnostic = only_error(&[("checks.toml", &fixed)]);
    assert_eq!(
        diagnostic.path.as_deref(),
        Some("check[0].http.headers.Authorization")
    );
    assert_eq!(
        diagnostic.message,
        "environment variable `MISSING_AUTHORIZATION` is not set"
    );
}

#[test]
fn tokens_need_their_own_secrets() {
    let text = r#"
[[token]]
id = "ci"
secret = "${ENV:TOKEN_A}"
scopes = ["release"]

[[token]]
id = "cron"
secret = "${ENV:TOKEN_C}"
scopes = ["heartbeat"]

[[token]]
id = "deploy"
secret = "${ENV:TOKEN_B}"
scopes = ["release"]
"#;
    let diagnostic = only_error(&[("tokens.toml", text)]);
    assert_eq!(diagnostic.location().as_deref(), Some("tokens.toml:14:10"));
    assert_eq!(
        diagnostic.message,
        "token `deploy` has the same secret as token `ci` at tokens.toml:4:10"
    );
    assert!(!diagnostic.to_string().contains("SHARED-TOKEN-SECRET"));
}

#[test]
fn resolvers_server_names_and_smtp_urls_are_checked() {
    let checks = r#"
[[check]]
id = "dns"
type = "dns"
target = "example.com"
[check.dns]
resolver = "dns.google"

[[check]]
id = "dns-port"
type = "dns"
target = "example.com"
[check.dns]
resolver = "[2001:db8::53]:53"

[[check]]
id = "tls"
type = "tls"
target = "192.0.2.1:443"
[check.tls]
server_name = "example com"
"#;
    let server = "[server.email]\nfrom = \"status@example.com\"\nreply_to = \"support@example.com\"\nsmtp_url = \"${ENV:HTTPS_URL}\"\n";
    let diagnostics =
        run(&[("checks.toml", checks), ("server.toml", server)]).expect_err("invalid");
    let shown: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "checks.toml:7:12: `dns.google` is not an IP address or ip:port (check[0].dns.resolver)\n  hint: such as \"1.1.1.1\", \"192.0.2.53:5353\" or \"[2001:db8::53]:53\"",
            "checks.toml:21:15: `example com` is not a hostname (check[2].tls.server_name)",
            "server.toml:4:12: the value of `HTTPS_URL` is not an smtp:// or smtps:// URL (server.email.smtp_url)\n  hint: such as smtps://user:password@mail.example.com:465",
        ]
    );
    assert!(!shown.concat().contains("HTTPS-SECRET"));
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
fn statuspage_filters_and_probe_settings_load() {
    let checks = r#"
[[check]]
id = "github-api"
type = "http"
target = "https://www.githubstatus.com/api/v2/components.json"
[check.http]
server_name = "www.githubstatus.com"
bearer_token = "${ENV:TOKEN_A}"

[[check.assert]]
json = { path = "$.components[?@.name=='API Requests'].status", eq = "operational" }
on_fail = "degraded"

[[check.assert]]
body = { regex = "\"status\":\\s*\"\\w+\"" }

[[check]]
id = "redis"
type = "tcp"
target = "cache.example.com:6379"
[check.tcp]
send = "PING\r\n"

[[check.assert]]
banner = { regex = "^\\+PONG$" }
"#;
    let config = run(&[("checks.toml", checks)]).expect("valid");
    let github = &config.checks[0];
    let http = github.http.as_ref().expect("http settings");
    assert_eq!(http.server_name.as_deref(), Some("www.githubstatus.com"));
    assert_eq!(
        http.bearer_token.as_ref().map(Secret::expose),
        Some("SHARED-TOKEN-SECRET")
    );
    assert!(http.verify_certificate);
    assert!(!http.follow_redirects);
    let tcp = config.checks[1].tcp.as_ref().expect("tcp settings");
    assert_eq!(tcp.send.as_deref(), Some("PING\r\n"));
}

#[test]
fn http_checks_verify_certificates_without_an_http_table() {
    let checks = "[[check]]\nid = \"web\"\ntype = \"http\"\ntarget = \"https://example.com\"\n";
    let config = run(&[("checks.toml", checks)]).expect("valid");
    let verify = config.checks[0]
        .http
        .as_ref()
        .is_none_or(|http| http.verify_certificate);
    assert!(verify);
}

#[test]
fn assertions_and_probe_settings_are_checked_when_loaded() {
    let checks = r#"
[[check]]
id = "web"
type = "http"
target = "http://example.com"
[check.http]
bearer_token = "${ENV:TOKEN_A}"
basic_auth = { username = "monitor", password = "${ENV:TOKEN_C}" }

[[check.assert]]
json = { path = "$.components[?@.name=='API'", eq = "operational" }

[[check.assert]]
body = { regex = "(unclosed" }

[[check.assert]]
certificate_days_remaining = { gt = 14 }

[[check]]
id = "cache"
type = "tcp"
target = "cache.example.com:6379"

[[check.assert]]
banner = { contains = "PONG", regex = "PONG" }
"#;
    assert_eq!(
        errors(&[("checks.toml", checks)]),
        [
            "checks.toml:8:14: give `bearer_token` or `basic_auth`, not both",
            "checks.toml:11:8: `$.components[?@.name=='API'` is not a JSONPath: at position 27, in long-hand segment, expected closing ']'",
            "checks.toml:14:8: `(unclosed` is not a regular expression: regex parse error:\n    (unclosed\n    ^\nerror: unclosed group",
            "checks.toml:17:30: a `certificate_days_remaining` assertion needs an https:// target, and this one is http://",
            "checks.toml:25:10: give exactly one of contains, not_contains or regex",
        ]
    );
}

#[test]
fn a_dotted_name_rfc_9535_rejects_gets_a_bracket_hint() {
    let check = |path: &str| {
        format!(
            "[[check]]\nid = \"api\"\ntype = \"http\"\ntarget = \"https://api.example.com\"\n\n[[check.assert]]\njson = {{ path = \"{path}\", exists = true }}\n"
        )
    };
    let hint = |path: &str| only_error(&[("checks.toml", &check(path))]).hint;
    assert_eq!(
        hint("$.headers.x-request-id").as_deref(),
        Some("use bracket notation: $.headers['x-request-id']")
    );
    assert_eq!(
        hint("$.items.0.name").as_deref(),
        Some("use bracket notation: $.items['0'].name")
    );
    assert_eq!(
        hint("$..x-request-id").as_deref(),
        Some("use bracket notation: $..['x-request-id']")
    );
    // Any other parse failure keeps the general examples.
    assert_eq!(
        hint("$.status[").as_deref(),
        Some("such as \"$.status\", \"$.checks[0].ok\" or \"$.components[?@.name=='API'].status\"")
    );
    // Strict RFC 9535: the bracketed form the hint suggests loads.
    run(&[("checks.toml", &check("$.headers['x-request-id']"))]).expect("valid");
    run(&[("checks.toml", &check("$..['x-request-id']"))]).expect("valid");
}

#[test]
fn headers_that_cannot_be_sent_are_rejected_without_echoing_secrets() {
    let checks = r#"
[[check]]
id = "api"
type = "http"
target = "https://api.example.com/health"
[check.http]
headers = { "Bad Name" = "x", Authorization = "${ENV:BROKEN_HEADER}", Accept = "text/plain\u0007" }

[[check]]
id = "bearer"
type = "http"
target = "https://api.example.com/health"
[check.http]
bearer_token = "${ENV:BROKEN_HEADER}"
"#;
    let diagnostics = run(&[("checks.toml", checks)]).expect_err("invalid");
    let shown: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "checks.toml:7:26: `Bad Name` is not an HTTP header name (check[0].http.headers.Bad Name)\n  hint: a header name is letters, digits and !#$%&'*+-.^_`|~, such as \"X-Request-Id\"",
            "checks.toml:7:47: the value of environment variable `BROKEN_HEADER` cannot be sent in an HTTP header: it holds a control character, such as a newline, or a character outside ASCII (check[0].http.headers.Authorization)",
            "checks.toml:7:80: this value cannot be sent in an HTTP header: it holds a control character, such as a newline, or a character outside ASCII (check[0].http.headers.Accept)",
            "checks.toml:14:16: the value of environment variable `BROKEN_HEADER` cannot be sent in an HTTP header: it holds a control character, such as a newline, or a character outside ASCII (check[1].http.bearer_token)",
        ]
    );
    let all = format!("{shown:?}{diagnostics:?}");
    assert!(
        !all.contains("LINE-SECRET") && !all.contains("Injected"),
        "{all}"
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
fn checks_and_heartbeats_carry_links() {
    let check = "[[check]]\nid = \"web\"\ntype = \"tcp\"\ntarget = \"example.com:443\"\n\n[[check.link]]\nname = \"Dashboard\"\nurl = \"https://grafana.example.com/d/web\"\n";
    let heartbeat = "[[heartbeat]]\nid = \"backup\"\ninterval = \"1d\"\n[[heartbeat.link]]\nname = \"Logs\"\nurl = \"http://logs.example.com/?q=backup\"\n";
    let config = run(&[("checks.toml", check), ("heartbeats.toml", heartbeat)]).expect("valid");
    assert_eq!(config.checks[0].links[0].name, "Dashboard");
    assert_eq!(
        config.checks[0].links[0].url,
        "https://grafana.example.com/d/web"
    );
    assert_eq!(
        config.heartbeats[0].links[0].url,
        "http://logs.example.com/?q=backup"
    );
    let shown = serde_json::to_value(&config).unwrap();
    assert_eq!(shown["check"][0]["link"][0]["name"], "Dashboard");
}

#[test]
fn links_need_a_name_and_an_absolute_http_url() {
    let text = "[[check]]\nid = \"web\"\ntype = \"tcp\"\ntarget = \"example.com:443\"\n\n[[check.link]]\nname = \"Dashboard\"\nurl = \"https://grafana.example.com/d/web\"\n\n[[check.link]]\nname = \" \"\nurl = \"/runbooks/web\"\n\n[[heartbeat]]\nid = \"backup\"\ninterval = \"1d\"\n\n[[heartbeat.link]]\nname = \"Logs\"\nurl = \"ftp://logs.example.com\"\n";
    assert_eq!(
        errors(&[("links.toml", text)]),
        [
            "links.toml:11:8: a link needs a non-empty `name`",
            "links.toml:12:7: `/runbooks/web` is not an http:// or https:// URL",
            "links.toml:20:7: `ftp://logs.example.com` is not an http:// or https:// URL",
        ]
    );
    let diagnostics = run(&[("links.toml", text)]).expect_err("invalid");
    let paths: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.path.as_deref())
        .collect();
    assert_eq!(
        paths,
        [
            Some("check[0].link[1].name"),
            Some("check[0].link[1].url"),
            Some("heartbeat[0].link[0].url")
        ]
    );

    let unknown = "[[heartbeat]]\nid = \"backup\"\ninterval = \"1d\"\n[[heartbeat.link]]\nname = \"Logs\"\nhref = \"https://logs.example.com\"\n";
    let diagnostic = only_error(&[("links.toml", unknown)]);
    assert_eq!(diagnostic.location().as_deref(), Some("links.toml:6:1"));
    assert!(
        diagnostic.message.starts_with("unknown field `href`"),
        "{}",
        diagnostic.message
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

#[test]
fn admin_auth_modes_load() {
    let config = run(&[(
        "tocsin.toml",
        r#"[server.auth]
mode = "cloudflare_access"
team_domain = "acme"
audience = "4714c1358e65fe4b408ad6d432a5f878f08194bdb4752441fd56faefa9b2b6f2"
allowed_domains = ["example.com"]

[server]
public_url = "https://status.example.com"
"#,
    )])
    .unwrap();
    let auth = config.server.auth.unwrap();
    assert_eq!(auth.mode, AuthMode::CloudflareAccess);
    assert_eq!(auth.allowed_domains, ["example.com"]);

    let config = run(&[(
        "tocsin.toml",
        r#"[server.auth]
mode = "trusted_header"
trusted_proxies = ["127.0.0.1", "fd00::/8"]
allowed_emails = ["alice@example.com"]

[server]
public_url = "https://status.example.com"
"#,
    )])
    .unwrap();
    let auth = config.server.auth.unwrap();
    assert_eq!(auth.mode, AuthMode::TrustedHeader);
    assert!(auth.trusted_proxies[0].contains("127.0.0.1".parse().unwrap()));

    // The table's absence and `mode = "none"` both leave the page off.
    assert!(run(&[]).unwrap().server.auth.is_none());
    let config = run(&[("tocsin.toml", "[server.auth]\n")]).unwrap();
    assert_eq!(config.server.auth.unwrap().mode, AuthMode::None);
}

#[test]
fn admin_auth_needs_the_settings_its_mode_relies_on() {
    assert_eq!(
        errors(&[(
            "tocsin.toml",
            "[server]\nlisten = \"127.0.0.1:8080\"\npublic_url = \"https://status.example.com\"\n\n[server.auth]\nmode = \"cloudflare_access\"\n",
        )]),
        [
            "tocsin.toml:5:1: `mode = \"cloudflare_access\"` needs `team_domain`",
            "tocsin.toml:5:1: `mode = \"cloudflare_access\"` needs `audience`",
        ]
    );
    assert_eq!(
        errors(&[(
            "tocsin.toml",
            "[server.auth]\nmode = \"trusted_header\"\ntrusted_proxies = []\n\n[server]\npublic_url = \"https://status.example.com\"\n",
        )]),
        [
            "tocsin.toml:1:1: `mode = \"trusted_header\"` needs `trusted_proxies`, the networks your proxy connects from"
        ]
    );
}

#[test]
fn admin_auth_rejects_settings_from_another_mode_and_bad_values() {
    assert_eq!(
        errors(&[(
            "tocsin.toml",
            r#"[server.auth]
mode = "cloudflare_access"
team_domain = "acme.example.com"
audience = "aud"
trusted_proxies = ["10.0.0.0/8"]
allowed_emails = ["Alice <alice@example.com>"]
allowed_domains = ["@example.com"]

[server]
public_url = "https://status.example.com"
"#,
        )]),
        [
            "tocsin.toml:3:15: `acme.example.com` is not a Cloudflare Access team: write `name` or `name.cloudflareaccess.com`",
            "tocsin.toml:5:19: `trusted_proxies` does not apply when `mode = \"cloudflare_access\"`",
            "tocsin.toml:6:19: `Alice <alice@example.com>` must be a bare address such as alice@example.com",
            "tocsin.toml:7:20: `@example.com` is not a domain name such as example.com",
        ]
    );
    assert_eq!(
        errors(&[(
            "tocsin.toml",
            "[server.auth]\nallowed_emails = [\"alice@example.com\"]\n",
        )]),
        ["tocsin.toml:2:18: `allowed_emails` does not apply when `mode = \"none\"`"]
    );
    assert_eq!(
        errors(&[(
            "tocsin.toml",
            r#"[server.auth]
mode = "trusted_header"
header = "Cf-Access-Authenticated-User-Email"
trusted_proxies = ["10.0.0.1/8"]

[server]
public_url = "https://status.example.com"
"#,
        )]),
        [
            "tocsin.toml:4:19: invalid network `10.0.0.1/8`: bits after the /8 prefix are set; did you mean `10.0.0.0/8`?",
        ]
    );
    // With the network fixed, the header itself is refused.
    assert_eq!(
        errors(&[(
            "tocsin.toml",
            r#"[server.auth]
mode = "trusted_header"
header = "Cf-Access-Authenticated-User-Email"
trusted_proxies = ["10.0.0.0/8"]

[server]
public_url = "https://status.example.com"
"#,
        )]),
        ["tocsin.toml:3:10: `Cf-Access-Authenticated-User-Email` proves nothing on its own"]
    );
}

#[test]
fn an_admin_page_needs_the_public_url_its_forms_are_checked_against() {
    assert_eq!(
        errors(&[(
            "tocsin.toml",
            "[server.auth]\nmode = \"trusted_header\"\ntrusted_proxies = [\"10.0.0.0/8\"]\n",
        )]),
        [
            "tocsin.toml:2:8: the admin page needs `[server] public_url`: its forms are accepted only from that origin"
        ]
    );
}

#[test]
fn a_team_domain_is_kept_in_lowercase() {
    // Access issues `iss` in lowercase; a mixed-case team would never match.
    for team in ["MyTeam", "MyTeam.CloudflareAccess.com"] {
        let config = run(&[(
            "tocsin.toml",
            &format!(
                "[server.auth]\nmode = \"cloudflare_access\"\nteam_domain = \"{team}\"\naudience = \"aud\"\n\n[server]\npublic_url = \"https://status.example.com\"\n"
            ),
        )])
        .unwrap();
        let domain = config.server.auth.unwrap().team_domain.unwrap();
        assert_eq!(domain, team.to_ascii_lowercase(), "{team}");
    }
}

#[test]
fn pushover_priorities_run_from_minus_two_to_two() {
    let pushover = |priorities: &str| {
        format!(
            "[[notifier]]\nid = \"phone\"\ntype = \"pushover\"\n[notifier.pushover]\nuser_key = \"${{ENV:TOKEN_A}}\"\napi_token = \"${{ENV:TOKEN_C}}\"\n{priorities}"
        )
    };
    let config = run(&[("notify.toml", &pushover("down_priority = 2\n"))]).expect("valid");
    let settings = config.notifiers[0].pushover.as_ref().expect("pushover");
    assert_eq!(
        (
            settings.down_priority,
            settings.degraded_priority,
            settings.resolved_priority
        ),
        (2, 0, 0)
    );
    assert_eq!(
        errors(&[(
            "notify.toml",
            &pushover("down_priority = 3\nresolved_priority = -3\n")
        )]),
        [
            "notify.toml:7:17: a Pushover priority is -2, -1, 0, 1 or 2",
            "notify.toml:8:21: a Pushover priority is -2, -1, 0, 1 or 2",
        ]
    );
}

#[test]
fn a_heartbeat_may_not_reuse_a_check_id() {
    let diagnostic = only_error(&[
        (
            "checks.toml",
            "[[check]]\nid = \"backup\"\ntype = \"icmp\"\ntarget = \"192.0.2.1\"\n",
        ),
        (
            "heartbeats.toml",
            "[[heartbeat]]\nid = \"backup\"\ninterval = \"1h\"\n",
        ),
    ]);
    assert_eq!(
        diagnostic.location().as_deref(),
        Some("heartbeats.toml:2:6")
    );
    assert_eq!(
        diagnostic.message,
        "heartbeat id `backup` is already a check id, defined at checks.toml:2:6"
    );
}

#[test]
fn routes_repeat_every_four_hours_by_default() {
    let notifier = "[[notifier]]\nid = \"ops\"\ntype = \"slack\"\n[notifier.slack]\nwebhook_url = \"${ENV:SLACK_WEBHOOK_URL}\"\n\n";
    let route =
        |extra: &str| format!("{notifier}[[route]]\nid = \"all\"\nnotifiers = [\"ops\"]\n{extra}");
    let config = run(&[("notify.toml", &route(""))]).expect("valid");
    assert_eq!(
        config.routes[0].repeat.get(),
        std::time::Duration::from_hours(4)
    );

    let config = run(&[("notify.toml", &route("repeat = \"0s\"\n"))]).expect("valid");
    assert!(config.routes[0].repeat.get().is_zero(), "0s never repeats");

    assert_eq!(
        errors(&[("notify.toml", &route("repeat = \"30s\"\n"))]),
        ["notify.toml:10:10: `repeat` must be at least 1m, or \"0s\" to never repeat"]
    );
}

#[test]
fn a_route_cannot_turn_resolves_off() {
    // Every alert resolves: there is no fire-only route.
    let notifier = "[[notifier]]\nid = \"ops\"\ntype = \"slack\"\n[notifier.slack]\nwebhook_url = \"${ENV:SLACK_WEBHOOK_URL}\"\n\n";
    let rejected = only_error(&[(
        "notify.toml",
        &format!("{notifier}[[route]]\nid = \"all\"\nnotifiers = [\"ops\"]\nresolve = false\n"),
    )]);
    assert!(
        rejected.message.starts_with("unknown field `resolve`"),
        "{rejected}"
    );
}

#[test]
fn a_notifier_sends_two_at_a_time_unless_told_otherwise() {
    let notifier = |extra: &str| {
        format!(
            "[[notifier]]\nid = \"ops\"\ntype = \"slack\"\n{extra}[notifier.slack]\nwebhook_url = \"${{ENV:SLACK_WEBHOOK_URL}}\"\n"
        )
    };
    let config = run(&[("notify.toml", &notifier(""))]).expect("valid");
    assert_eq!(config.notifiers[0].max_in_flight, 2);
    let config = run(&[("notify.toml", &notifier("max_in_flight = 8\n"))]).expect("valid");
    assert_eq!(config.notifiers[0].max_in_flight, 8);
    assert_eq!(
        errors(&[("notify.toml", &notifier("max_in_flight = 0\n"))]),
        ["notify.toml:4:17: `max_in_flight` must be at least 1"]
    );
}

#[test]
fn a_webhook_signing_secret_is_required_and_redacted() {
    let webhook = |secret: &str| {
        format!(
            "[[notifier]]\nid = \"hook\"\ntype = \"webhook\"\n[notifier.webhook]\nurl = \"https://hooks.example.com/tocsin\"\n{secret}"
        )
    };
    let config = run(&[(
        "notify.toml",
        &webhook("signing_secret = \"${ENV:WEBHOOK_SECRET}\"\n"),
    )])
    .expect("valid");
    let settings = config.notifiers[0].webhook.as_ref().expect("webhook");
    assert_eq!(settings.signing_secret.expose(), "SIGNING-SECRET");
    let shown = format!(
        "{} {config:?}",
        serde_json::to_string(&config).expect("serializes")
    );
    assert!(!shown.contains("SIGNING-SECRET"), "{shown}");

    let missing = only_error(&[("notify.toml", &webhook(""))]);
    assert!(missing.message.contains("signing_secret"), "{missing}");
    let hint = missing.hint.as_deref().unwrap_or_default();
    assert!(hint.contains("openssl rand -hex 32"), "{missing}");
    assert!(hint.contains("${ENV:NAME}"), "{missing}");
    let pasted = only_error(&[(
        "notify.toml",
        &webhook("signing_secret = \"PASTED-SIGNING-SECRET\"\n"),
    )]);
    assert_eq!(pasted.location().as_deref(), Some("notify.toml:6:18"));
    assert!(
        !pasted.to_string().contains("PASTED-SIGNING-SECRET"),
        "{pasted}"
    );
}

#[test]
fn a_heartbeat_must_wait_well_inside_retention() {
    let server = "[server]\nretention_days = 7\n";
    // Exactly half the retention is allowed.
    run(&[
        ("server.toml", server),
        (
            "heartbeats.toml",
            "[[heartbeat]]\nid = \"backup\"\ninterval = \"3d\"\ngrace = \"12h\"\n",
        ),
    ])
    .expect("valid");
    let diagnostic = only_error(&[
        ("server.toml", server),
        (
            "heartbeats.toml",
            "[[heartbeat]]\nid = \"backup\"\ninterval = \"3d\"\ngrace = \"12h1s\"\n",
        ),
    ]);
    assert_eq!(
        diagnostic.location().as_deref(),
        Some("heartbeats.toml:3:12")
    );
    assert_eq!(
        diagnostic.message,
        "heartbeat `backup`: `interval` (3d) + `grace` (12h1s) must be at most half of `[server] retention_days = 7` (server.toml:2:18)"
    );
    // Unset, the default retention applies, and the message says so.
    let diagnostic = only_error(&[(
        "heartbeats.toml",
        "[[heartbeat]]\nid = \"backup\"\ninterval = \"45d\"\n",
    )]);
    assert_eq!(
        diagnostic.message,
        "heartbeat `backup`: `interval` (45d) + `grace` (1m) must be at most half of the default `[server] retention_days = 90`"
    );
}

#[test]
fn the_page_takes_a_logo_links_an_order_and_a_time_zone() {
    let page = r#"
[page]
title = "Acme status"
groups = ["data", "edge"]
components = ["db", "api"]
timezone = "Europe/Berlin"

[page.logo]
url = "/brand/logo.svg"
alt = "Acme"

[[page.link]]
name = "Support"
url = "https://acme.example/support"
"#;
    let components = "[[group]]\nid = \"edge\"\nname = \"Edge\"\n\n[[group]]\nid = \"data\"\nname = \"Data\"\n\n\
                      [[component]]\nid = \"api\"\nname = \"API\"\n\n[[component]]\nid = \"db\"\nname = \"DB\"\n";
    let config = run(&[("page.toml", page), ("components.toml", components)]).unwrap();
    let logo = config.page.logo.as_ref().unwrap();
    assert_eq!(
        (logo.url.as_str(), logo.alt.as_str()),
        ("/brand/logo.svg", "Acme")
    );
    assert_eq!(config.page.links[0].name, "Support");
    let ids = |ids: &[tocsin_core::config::Id]| -> Vec<String> {
        ids.iter().map(|id| id.as_str().to_owned()).collect()
    };
    assert_eq!(ids(&config.page.groups), ["data", "edge"]);
    assert_eq!(ids(&config.page.components), ["db", "api"]);
    assert_eq!(config.page.timezone, "Europe/Berlin");
    // Unset, the page shows UTC.
    assert_eq!(run(&[]).unwrap().page.timezone, "UTC");
}

#[test]
fn an_accent_sets_both_schemes() {
    // Set alone, the dark scheme's is derived and still reads.
    let config = run(&[("page.toml", "[page]\naccent = \"#7A1FA2\"\n")]).expect("valid");
    let (light, dark) = config.page.accents();
    assert_eq!(
        light.map(|colour| colour.to_string()).as_deref(),
        Some("#7a1fa2")
    );
    let dark = dark.expect("derived");
    assert_ne!(dark, light.unwrap());
    assert!(DARK.passes(dark), "{dark}");
    // Given, the dark one is used as written.
    let config = run(&[(
        "page.toml",
        "[page]\naccent = \"#06c\"\naccent_dark = \"#58a6ff\"\n",
    )])
    .expect("valid");
    let shown = config.page.accents();
    assert_eq!(
        (shown.0.unwrap().to_string(), shown.1.unwrap().to_string()),
        ("#0066cc".to_owned(), "#58a6ff".to_owned())
    );
    // Unset, the stylesheet keeps its own.
    assert_eq!(run(&[]).unwrap().page.accents(), (None, None));
}

#[test]
fn an_accent_that_is_not_a_hex_colour_is_located_at_its_value() {
    let diagnostic = only_error(&[("page.toml", "[page]\naccent = \"rebeccapurple\"\n")]);
    assert_eq!(
        diagnostic.to_string(),
        "page.toml:2:10: invalid colour `rebeccapurple`: write it in hex, such as \"#0b5cad\" or \"#06c\" (page.accent)"
    );
}

#[test]
fn an_accent_too_light_for_the_light_scheme_is_refused() {
    let diagnostic = only_error(&[("page.toml", "[page]\naccent = \"#ffcc00\"\n")]);
    assert_eq!(
        diagnostic.to_string(),
        "page.toml:2:10: `#ffcc00` has 1.42:1 contrast with the light scheme's background #f6f8fa; text needs at least 4.5:1 (WCAG AA) (page.accent)\n  hint: choose a darker colour"
    );
}

#[test]
fn a_dark_accent_too_dark_for_the_dark_scheme_is_refused() {
    let page = "[page]\naccent = \"#0b5cad\"\naccent_dark = \"#1f6feb\"\n";
    let diagnostic = only_error(&[("page.toml", page)]);
    assert_eq!(
        diagnostic.to_string(),
        "page.toml:3:15: `#1f6feb` has 3.73:1 contrast with the dark scheme's surface #161b22; text needs at least 4.5:1 (WCAG AA) (page.accent_dark)\n  hint: choose a lighter colour, or leave `accent_dark` out to derive it from `accent`"
    );
}

#[test]
fn page_settings_are_checked_when_loaded() {
    let page = r#"
[page]
groups = ["edge", "egde", "edge"]
components = ["api", "api"]
timezone = "Mars/Olympus"

[page.logo]
url = "https://cdn.example.com/logo.svg"
alt = "Acme"

[[page.link]]
name = " "
url = "support.example.com"
"#;
    let components = "[[group]]\nid = \"edge\"\nname = \"Edge\"\n\n[[component]]\nid = \"api\"\nname = \"API\"\n";
    let diagnostics =
        run(&[("page.toml", page), ("components.toml", components)]).expect_err("invalid");
    let shown: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "page.toml:3:19: unknown group `egde` (page.groups[1])\n  hint: did you mean `edge`?",
            "page.toml:3:27: `edge` is listed twice (page.groups[2])",
            "page.toml:4:22: `api` is listed twice (page.components[1])",
            "page.toml:5:12: unknown time zone `Mars/Olympus` (page.timezone)\n  hint: use an IANA name such as \"Europe/Berlin\", or \"UTC\"",
            "page.toml:8:7: `https://cdn.example.com/logo.svg` is not a path on this site, such as \"/brand/logo.svg\" (page.logo.url)\n  hint: the page loads images from its own origin only; serve the logo from the proxy in front of tocsin",
            "page.toml:12:8: a link needs a non-empty `name` (page.link[0].name)",
            "page.toml:13:7: `support.example.com` is not an http:// or https:// URL (page.link[0].url)",
        ]
    );
}

/// The contract Closient's alert sweep routes on, as a configuration would
/// declare it.
const CONTRACT: &str = r#"
[alerting.labels.needs_human]
required = true
values = ["false", "true"]

[alerting.labels.page_class]
required = true
values = ["page", "digest"]

[alerting.labels.remediation_action]

[alerting.annotations.ticket]
required = true
"#;

/// A check and a heartbeat carrying everything [`CONTRACT`] asks for.
const LABELLED: &str = r#"
[[check]]
id = "api"
type = "http"
target = "https://api.example.com/health"

[check.labels]
needs_human = "false"
page_class = "page"
team = "web"

[check.annotations]
ticket = "OPS-1"

[[heartbeat]]
id = "backup"
interval = "1h"
labels = { needs_human = "true", page_class = "digest" }
annotations = { ticket = "OPS-2", runbook = "https://example.com/backup" }
"#;

#[test]
fn labels_and_annotations_load_free_form_without_a_contract() {
    let config = run(&[("checks.toml", LABELLED)]).expect("valid");
    assert_eq!(config.alerting, None);
    let check = &config.checks[0];
    assert_eq!(check.labels["page_class"], "page");
    assert_eq!(check.labels["team"], "web");
    assert_eq!(check.annotations["ticket"], "OPS-1");
    assert_eq!(config.heartbeats[0].labels["needs_human"], "true");

    // Unlabelled subjects, and a label the contract would refuse, are
    // fine while no contract is declared.
    let loose = LABELLED.replace(r#"needs_human = "false""#, r#"needs_human = "ture""#);
    run(&[("checks.toml", &loose)]).expect("no contract, no check");
}

#[test]
fn a_contract_the_subjects_meet_loads() {
    let config = run(&[("alerting.toml", CONTRACT), ("checks.toml", LABELLED)]).expect("valid");
    let alerting = config.alerting.expect("declared");
    assert!(alerting.labels["needs_human"].required);
    assert_eq!(alerting.labels["page_class"].values, ["page", "digest"]);
    assert!(!alerting.labels["remediation_action"].required);
    assert!(alerting.annotations["ticket"].required);
}

#[test]
fn a_missing_required_label_names_the_file_the_check_and_the_label() {
    let text = LABELLED.replace("needs_human = \"false\"\n", "");
    let diagnostic = only_error(&[("alerting.toml", CONTRACT), ("checks.toml", &text)]);
    assert_eq!(
        diagnostic.to_string(),
        "checks.toml:7:1: check `api` is missing label `needs_human`, which `[alerting]` requires (check[0].labels.needs_human)\n  hint: add `needs_human = \"...\"` to its `labels`; it must be one of `false`, `true`"
    );
}

#[test]
fn a_misspelt_required_label_is_named_with_the_spelling_found() {
    let text = LABELLED.replace("needs_human = \"false\"", "neds_human = \"false\"");
    let diagnostic = only_error(&[("alerting.toml", CONTRACT), ("checks.toml", &text)]);
    assert_eq!(diagnostic.file.as_deref(), Some("checks.toml"));
    assert_eq!(
        diagnostic.path.as_deref(),
        Some("check[0].labels.needs_human")
    );
    assert_eq!(
        diagnostic.message,
        "check `api` is missing label `needs_human`, which `[alerting]` requires"
    );
    assert_eq!(
        diagnostic.hint.as_deref(),
        Some("`neds_human` is set: did you mean `needs_human`?")
    );
}

#[test]
fn a_label_value_outside_the_contract_is_located_at_the_value() {
    let text = LABELLED.replace("needs_human = \"false\"", "needs_human = \"ture\"");
    let diagnostic = only_error(&[("alerting.toml", CONTRACT), ("checks.toml", &text)]);
    assert_eq!(
        diagnostic.to_string(),
        "checks.toml:8:15: check `api`: label `needs_human` is `ture`, which `[alerting]` does not allow (check[0].labels.needs_human)\n  hint: did you mean `true`? It must be one of `false`, `true`"
    );
}

#[test]
fn an_empty_value_the_contract_covers_is_one_error_naming_the_values_it_allows() {
    let text = LABELLED.replace("needs_human = \"false\"", "needs_human = \"\"");
    let diagnostic = only_error(&[("alerting.toml", CONTRACT), ("checks.toml", &text)]);
    assert_eq!(
        diagnostic.to_string(),
        "checks.toml:8:15: check `api`: label `needs_human` is empty (check[0].labels.needs_human)\n  hint: Alertmanager drops an empty label: give it a value or remove it; it must be one of `false`, `true`"
    );
}

#[test]
fn the_contract_holds_heartbeats_and_annotations_too() {
    let text = LABELLED
        .replace(r#"page_class = "digest""#, r#"page_class = "pager""#)
        .replace(r#"ticket = "OPS-2", "#, "");
    let shown: Vec<String> = run(&[("alerting.toml", CONTRACT), ("checks.toml", &text)])
        .expect_err("invalid")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        shown,
        [
            "checks.toml:18:47: heartbeat `backup`: label `page_class` is `pager`, which `[alerting]` does not allow (heartbeat[0].labels.page_class)\n  hint: did you mean `page`? It must be one of `page`, `digest`",
            "checks.toml:19:15: heartbeat `backup` is missing annotation `ticket`, which `[alerting]` requires (heartbeat[0].annotations.ticket)\n  hint: add `ticket = \"...\"` to its `annotations`",
        ]
    );
}

#[test]
fn a_subject_with_no_labels_at_all_fails_every_required_one() {
    let text = "[[check]]\nid = \"bare\"\ntype = \"icmp\"\ntarget = \"192.0.2.1\"\n";
    let errors = errors(&[("alerting.toml", CONTRACT), ("checks.toml", text)]);
    assert_eq!(
        errors,
        [
            "checks.toml:1:1: check `bare` is missing label `needs_human`, which `[alerting]` requires",
            "checks.toml:1:1: check `bare` is missing label `page_class`, which `[alerting]` requires",
            "checks.toml:1:1: check `bare` is missing annotation `ticket`, which `[alerting]` requires",
        ]
    );
}

#[test]
fn label_names_must_be_names_alertmanager_takes_and_tocsin_does_not_set() {
    let text = r#"
[[check]]
id = "api"
type = "icmp"
target = "192.0.2.1"
labels = { "needs-human" = "true", subject_id = "other", team = "" }
annotations = { summary = "mine" }
"#;
    let shown: Vec<String> = run(&[("checks.toml", text)])
        .expect_err("invalid")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        shown,
        [
            "checks.toml:6:28: check `api`: `needs-human` is not a valid label name (check[0].labels.needs-human)\n  hint: use letters, digits and underscores, not starting with a digit or `__`, as Alertmanager requires",
            "checks.toml:6:49: check `api`: label `subject_id` is set by tocsin (check[0].labels.subject_id)\n  hint: tocsin sets `alertname`, `node`, `subject_kind`, `subject_id`, `class`, `topology_node` on every alert; choose another name",
            "checks.toml:6:65: check `api`: label `team` is empty (check[0].labels.team)\n  hint: Alertmanager drops an empty label: give it a value or remove it",
            "checks.toml:7:27: check `api`: annotation `summary` is set by tocsin (check[0].annotations.summary)\n  hint: tocsin sets `summary`, `description`, `health`, `name`, `component`, `component_name`, `tocsin_test`, `ended_by` on every alert; choose another name",
        ]
    );
}

#[test]
fn a_probe_s_class_is_one_tocsin_knows_and_production_unless_set() {
    let config = run(&[(
        "checks.toml",
        r#"
[[check]]
id = "api"
type = "icmp"
target = "192.0.2.1"

[[check]]
id = "drill"
type = "icmp"
target = "192.0.2.2"
class = "drill"

[[heartbeat]]
id = "backup"
interval = "1h"
class = "canary"
"#,
    )])
    .expect("valid");
    let classes: Vec<ProbeClass> = config.checks.iter().map(|check| check.class).collect();
    assert_eq!(classes, [ProbeClass::Production, ProbeClass::Drill]);
    assert_eq!(config.heartbeats[0].class, ProbeClass::Canary);

    let text =
        "[[check]]\nid = \"api\"\ntype = \"icmp\"\ntarget = \"192.0.2.1\"\nclass = \"exercise\"\n";
    let shown = errors(&[("checks.toml", text)]);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(shown[0].starts_with("checks.toml:5:"), "{shown:?}");
    assert!(shown[0].contains("exercise"), "{shown:?}");
}

#[test]
fn a_class_label_points_at_the_class_setting() {
    let text = r#"
[[check]]
id = "api"
type = "icmp"
target = "192.0.2.1"
labels = { class = "drill" }
"#;
    let shown: Vec<String> = run(&[("checks.toml", text)])
        .expect_err("invalid")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        shown,
        [
            "checks.toml:6:20: check `api`: label `class` is set by tocsin (check[0].labels.class)\n  hint: set the check's or heartbeat's `class` instead",
        ]
    );
}

#[test]
fn a_probe_s_topology_node_is_well_formed_and_optional() {
    let config = run(&[(
        "checks.toml",
        r#"
[[check]]
id = "api"
type = "icmp"
target = "192.0.2.1"
topology_node = "operations-01"

[[check]]
id = "web"
type = "icmp"
target = "192.0.2.2"

[[heartbeat]]
id = "backup"
interval = "1h"
topology_node = "operations-01/job:backup"
"#,
    )])
    .expect("valid");
    let nodes: Vec<Option<&str>> = config
        .checks
        .iter()
        .map(|check| check.topology_node.as_ref().map(TopologyNode::as_str))
        .collect();
    assert_eq!(nodes, [Some("operations-01"), None]);
    assert_eq!(
        config.heartbeats[0]
            .topology_node
            .as_ref()
            .map(TopologyNode::as_str),
        Some("operations-01/job:backup")
    );

    let text = "[[check]]\nid = \"api\"\ntype = \"icmp\"\ntarget = \"192.0.2.1\"\ntopology_node = \"Operations 01\"\n";
    let shown = errors(&[("checks.toml", text)]);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(shown[0].starts_with("checks.toml:5:"), "{shown:?}");
    assert!(shown[0].contains("invalid topology node"), "{shown:?}");
}

#[test]
fn a_topology_node_label_points_at_the_topology_node_setting() {
    let text = r#"
[[check]]
id = "api"
type = "icmp"
target = "192.0.2.1"
labels = { topology_node = "operations-01" }
"#;
    let shown: Vec<String> = run(&[("checks.toml", text)])
        .expect_err("invalid")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        shown,
        [
            "checks.toml:6:28: check `api`: label `topology_node` is set by tocsin (check[0].labels.topology_node)\n  hint: set the check's or heartbeat's `topology_node` instead",
        ]
    );
}

#[test]
fn with_a_topology_only_its_nodes_are_allowed() {
    const TOPOLOGY: &str =
        "[topology]\nnodes = [\"operations-01\", \"production-01\", \"production-01/web-a\"]\n";
    let probes = r#"
[[check]]
id = "api"
type = "icmp"
target = "192.0.2.1"
topology_node = "production-01/web-a"

[[check]]
id = "ops"
type = "icmp"
target = "192.0.2.2"
topology_node = "operatons-01"

[[heartbeat]]
id = "backup"
interval = "1h"
topology_node = "replication-03"
"#;
    let shown: Vec<String> = run(&[("checks.toml", probes), ("topology.toml", TOPOLOGY)])
        .expect_err("invalid")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        shown,
        [
            "checks.toml:12:17: check `ops`: topology node `operatons-01` is not in `[topology]` (check[1].topology_node)\n  hint: did you mean `operations-01`?",
            "checks.toml:17:17: heartbeat `backup`: topology node `replication-03` is not in `[topology]` (heartbeat[0].topology_node)\n  hint: it must be one of the 3 nodes `[topology]` lists; add it there if it is new",
        ]
    );

    // The same probes load once the map knows their nodes, and without a
    // `[topology]` any well-formed id goes.
    let known =
        "[topology]\nnodes = [\"production-01/web-a\", \"operatons-01\", \"replication-03\"]\n";
    let config = run(&[("checks.toml", probes), ("topology.toml", known)]).expect("valid");
    assert_eq!(config.topology.expect("loaded").nodes.len(), 3);
    run(&[("checks.toml", probes)]).expect("valid without [topology]");

    let bad = only_error(&[("topology.toml", "[topology]\nnodes = [\"Operations 01\"]\n")]);
    assert!(bad.message.contains("invalid topology node"), "{bad}");
    let twice = only_error(&[("one.toml", TOPOLOGY), ("two.toml", TOPOLOGY)]);
    assert!(
        twice
            .message
            .starts_with("`[topology]` is already defined at one.toml"),
        "{twice}"
    );
}

#[test]
fn the_contract_itself_is_checked() {
    let text = r#"
[alerting.labels.node]
required = true

[alerting.labels.page_class]
values = ["page", ""]

[alerting.annotations."run book"]
"#;
    let errors = errors(&[("alerting.toml", text)]);
    assert_eq!(
        errors,
        [
            "alerting.toml:2:1: label `node` is set by tocsin",
            "alerting.toml:6:10: label `page_class` cannot allow an empty value",
            "alerting.toml:8:1: `run book` is not a valid annotation name",
        ]
    );
    let twice = only_error(&[("one.toml", CONTRACT), ("two.toml", CONTRACT)]);
    assert!(
        twice
            .message
            .starts_with("`[alerting]` is already defined at one.toml"),
        "{twice}"
    );
}

fn run_as(node: Option<&str>, files: &[(&str, &str)]) -> Result<Config, Vec<Diagnostic>> {
    let sources: Vec<Source> = files
        .iter()
        .map(|(name, text)| Source {
            name: (*name).to_owned(),
            text: Ok((*text).to_owned()),
        })
        .collect();
    let environment = Environment {
        hostname: HOSTNAME.to_owned(),
        node_id: node.map(str::to_owned),
        variable: &variable,
    };
    load(&sources, &environment)
}

const NODES: &str = "[[node]]\nid = \"probe-a\"\n\n[[node]]\nid = \"probe-b\"\n\n[federation]\ndefault_nodes = [\"probe-a\", \"probe-b\"]\n";

#[test]
fn a_single_server_is_named_by_its_hostname_unless_told_otherwise() {
    let config = run(&[]).expect("valid");
    assert_eq!(config.server.node_id.as_deref(), Some(HOSTNAME));
    assert_eq!(config.node_id_source, NodeIdSource::Hostname);
    assert_eq!(config.nodes.len(), 0);

    let config = run(&[("server.toml", "[server]\nnode_id = \"old-host\"\n")]).expect("valid");
    assert_eq!(config.server.node_id.as_deref(), Some("old-host"));
    assert_eq!(config.node_id_source, NodeIdSource::Config);

    let config = run_as(
        Some("from-flag"),
        &[("server.toml", "[server]\nnode_id = \"old-host\"\n")],
    )
    .expect("valid");
    assert_eq!(config.server.node_id.as_deref(), Some("from-flag"));
    assert_eq!(config.node_id_source, NodeIdSource::Argument);
}

#[test]
fn listed_nodes_load_and_the_server_runs_as_one_of_them() {
    let config = run_as(Some("probe-b"), &[("nodes.toml", NODES)]).expect("valid");
    assert_eq!(config.server.node_id.as_deref(), Some("probe-b"));
    let ids: Vec<&str> = config.nodes.iter().map(|node| node.id.as_str()).collect();
    assert_eq!(ids, ["probe-a", "probe-b"]);

    let config = run(&[
        ("nodes.toml", NODES),
        ("server.toml", "[server]\nnode_id = \"probe-a\"\n"),
    ])
    .expect("valid");
    assert_eq!(config.server.node_id.as_deref(), Some("probe-a"));
}

#[test]
fn listed_nodes_need_the_node_id_given_not_defaulted() {
    let diagnostic = only_error(&[("nodes.toml", NODES)]);
    assert_eq!(diagnostic.location().as_deref(), Some("nodes.toml:1:1"));
    assert_eq!(
        diagnostic.message,
        "`[[node]]` is listed, so this server's node id must be given; \
         it would otherwise be the hostname `probe-01`"
    );
    assert_eq!(
        diagnostic.hint.as_deref(),
        Some(
            "set `node_id` in `[server]`, or pass `--node <id>` or TOCSIN_NODE_ID \
             (a running server takes those only at restart), to one of `probe-a`, `probe-b`"
        )
    );
}

#[test]
fn an_unlisted_node_id_is_located_with_a_suggestion() {
    let diagnostic = only_error(&[
        ("nodes.toml", NODES),
        ("server.toml", "[server]\nnode_id = \"probe-c\"\n"),
    ]);
    assert_eq!(diagnostic.location().as_deref(), Some("server.toml:2:11"));
    assert_eq!(
        diagnostic.message,
        "`node_id` `probe-c` is not a listed `[[node]]`"
    );
    assert!(diagnostic.hint.expect("hint").contains("did you mean"));

    let diagnostics = run_as(Some("elsewhere"), &[("nodes.toml", NODES)])
        .expect_err("configuration should be invalid");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].file, None);
    assert_eq!(diagnostics[0].path.as_deref(), Some("node_id"));
    assert_eq!(
        diagnostics[0].message,
        "the node id given by `--node` or `TOCSIN_NODE_ID` `elsewhere` is not a listed `[[node]]`"
    );
}

#[test]
fn a_node_id_must_be_an_id_once_nodes_are_listed() {
    let diagnostic = only_error(&[
        ("nodes.toml", NODES),
        ("server.toml", "[server]\nnode_id = \"Probe.A\"\n"),
    ]);
    assert_eq!(diagnostic.location().as_deref(), Some("server.toml:2:11"));
    assert!(
        diagnostic
            .message
            .starts_with("`node_id` `Probe.A` is not a node id: "),
        "{}",
        diagnostic.message
    );

    let diagnostic = only_error(&[
        ("nodes.toml", NODES),
        ("server.toml", "[server]\nnode_id = \"probe a\"\n"),
    ]);
    assert_eq!(
        diagnostic.message,
        "`node_id` must be non-empty with no whitespace"
    );

    let diagnostics = run_as(Some(" "), &[]).expect_err("configuration should be invalid");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "the node id given by `--node` or `TOCSIN_NODE_ID` must be non-empty with no whitespace"
    );
}

#[test]
fn a_node_is_listed_once() {
    let mut diagnostics = run_as(
        Some("probe-a"),
        &[
            ("a.toml", "[[node]]\nid = \"probe-a\"\n"),
            ("b.toml", "[[node]]\nid = \"probe-a\"\n"),
            ("c.toml", "[federation]\ndefault_nodes = [\"probe-a\"]\n"),
        ],
    )
    .expect_err("configuration should be invalid");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let diagnostic = diagnostics.remove(0);
    assert_eq!(diagnostic.location().as_deref(), Some("b.toml:2:6"));
    assert!(
        diagnostic
            .message
            .starts_with("duplicate node id `probe-a`"),
        "{}",
        diagnostic.message
    );
}

/// The public key of the node key in `NODE_KEY_A` or `NODE_KEY_B`.
fn public(seed: u8) -> String {
    NodeKey::from_seed([seed; 32]).public_key().to_string()
}

/// Two nodes, `central` taking pushes from `dmz-1`, each with its own key.
fn federation(node_key: &str) -> String {
    format!(
        "[server]\nnode_id = \"dmz-1\"\nnode_key = \"${{ENV:{node_key}}}\"\n\n\
         [[node]]\nid = \"central\"\nurl = \"https://central.example.com\"\npublic_keys = [\"{a}\"]\n\n\
         [[node]]\nid = \"dmz-1\"\npublic_keys = [\"{b}\"]\npush_to = [\"central\"]\n\n\
         [federation]\ndefault_nodes = [\"central\"]\nalert_order = [\"central\", \"dmz-1\"]\n",
        a = public(1),
        b = public(2),
    )
}

#[test]
fn a_federation_loads_with_each_node_signing_with_its_own_key() {
    let text = federation("NODE_KEY_B");
    let config = run(&[("server.toml", &text)]).expect("valid");
    let federation = config.federation.as_ref().expect("[federation]");
    assert_eq!(federation.default_nodes, [Id::parse("central").unwrap()]);
    assert_eq!(federation.push_interval.get().as_secs(), 15);
    assert_eq!(federation.grace.get().as_secs(), 60);
    let dmz = &config.nodes[1];
    assert_eq!(dmz.push_to, [Id::parse("central").unwrap()]);
    assert!(!dmz.retired);
    let key = NodeKey::parse(config.server.node_key.as_ref().unwrap().expose()).unwrap();
    assert!(dmz.public_keys.contains(&key.public_key()));

    // The key is shown by where it came from, never by its value.
    let shown = serde_json::to_string(&config).unwrap();
    assert!(
        shown.contains("<redacted from ${ENV:NODE_KEY_B}>"),
        "{shown}"
    );
    assert!(!shown.contains(&NodeKey::from_seed([2; 32]).expose()));
}

#[test]
fn a_node_key_copied_to_the_wrong_host_names_the_node_it_runs_as() {
    let text = federation("NODE_KEY_A");
    let diagnostic = only_error(&[("server.toml", &text)]);
    assert_eq!(diagnostic.location().as_deref(), Some("server.toml:3:12"));
    assert_eq!(
        diagnostic.message,
        format!(
            "the key in `NODE_KEY_A` is not listed under node `dmz-1`: its public key is {}",
            public(1)
        )
    );

    let text = federation("NODE_KEY_BROKEN");
    let diagnostic = only_error(&[("server.toml", &text)]);
    assert!(
        diagnostic
            .message
            .starts_with("the value of `NODE_KEY_BROKEN` is not a node key"),
        "{}",
        diagnostic.message
    );
    assert!(!diagnostic.to_string().contains("NODE-KEY-SECRET"));
}

#[test]
fn a_node_that_pushes_needs_a_node_key() {
    let text = federation("NODE_KEY_B").replace("node_key = \"${ENV:NODE_KEY_B}\"\n", "");
    let diagnostic = only_error(&[("server.toml", &text)]);
    assert!(
        diagnostic
            .message
            .starts_with("node `dmz-1` pushes to other nodes, so `[server] node_key` is required"),
        "{}",
        diagnostic.message
    );
}

#[test]
fn federation_and_nodes_come_together() {
    assert_eq!(
        errors(&[(
            "a.toml",
            "[server]\nnode_id = \"probe-a\"\n[[node]]\nid = \"probe-a\"\n"
        )]),
        ["a.toml:3:1: `[[node]]` is listed, so `[federation]` is required"]
    );
    assert_eq!(
        errors(&[("a.toml", "[federation]\ndefault_nodes = []\n")]),
        ["a.toml:1:1: `[federation]` needs at least one `[[node]]`"]
    );
    assert_eq!(
        errors(&[("a.toml", "[server]\nnode_key = \"${ENV:NODE_KEY_A}\"\n")]),
        ["a.toml:2:12: `node_key` signs pushes to other nodes, and no `[[node]]` is listed"]
    );
}

#[test]
fn federation_references_are_checked() {
    let text = format!(
        "[server]\nnode_id = \"a\"\n\n\
         [[node]]\nid = \"a\"\npush_to = [\"a\", \"b\", \"c\", \"c\"]\npublic_keys = [\"{one}\", \"{one}\"]\n\n\
         [[node]]\nid = \"b\"\npublic_keys = [\"{two}\"]\n\n\
         [[node]]\nid = \"c\"\nurl = \"https://c.example.com\"\nretired = true\npublic_keys = [\"{two}\"]\n\n\
         [federation]\ndefault_nodes = [\"a\", \"a\", \"c\"]\nalert_order = [\"d\"]\npush_interval = \"0s\"\n",
        one = public(1),
        two = public(2),
    );
    let found = errors(&[("f.toml", &text)]);
    let expected = [
        "f.toml:6:11: node `a` pushes to other nodes, so `[server] node_key` is required to sign its pushes".to_owned(),
        "f.toml:6:12: a node does not push to itself".to_owned(),
        "f.toml:6:17: node `b` has no `url` to take pushes at".to_owned(),
        "f.toml:6:22: node `c` is retired".to_owned(),
        "f.toml:6:27: `c` is listed twice".to_owned(),
        format!("f.toml:7:{}: public key `{}` is listed twice", 16 + public(1).len() + 4, public(1)),
        format!(
            "f.toml:17:16: public key `{}` is also listed under node `b` at f.toml:11:16",
            public(2)
        ),
        "f.toml:20:23: `a` is listed twice".to_owned(),
        "f.toml:20:28: node `c` is retired".to_owned(),
        "f.toml:21:16: unknown node `d`".to_owned(),
        "f.toml:22:17: `push_interval` must be longer than 0s".to_owned(),
    ];
    assert_eq!(found, expected);
}

/// [`federation`] with a check `api` run on both nodes every 30 seconds,
/// and `extra` added to its table.
fn quorum_check(extra: &str) -> String {
    format!(
        "{}\n[[check]]\nid = \"api\"\ntype = \"tcp\"\ntarget = \"api.example.com:443\"\n\
         interval = \"30s\"\nnodes = [\"central\", \"dmz-1\"]\n{extra}",
        federation("NODE_KEY_B")
    )
}

#[test]
fn a_check_on_two_nodes_is_a_quorum_check_derived_where_its_results_converge() {
    let config = run(&[("server.toml", &quorum_check(""))]).expect("valid");
    let check = &config.checks[0];
    assert_eq!(config.quorum(check), Some(2), "a majority of two");
    assert!(
        config.evaluates_quorum(check, "central"),
        "dmz-1 pushes to it"
    );
    assert!(!config.evaluates_quorum(check, "dmz-1"));
    assert_eq!(
        config.quorum_evaluators(check),
        [&Id::parse("central").unwrap()]
    );
    // Two intervals, then a push interval and grace for the last to arrive.
    assert_eq!(config.quorum_window(check).as_secs(), 30 * 2 + 15 + 60);

    let heartbeats = config.quorum_heartbeats("central");
    assert_eq!(heartbeats.len(), 1);
    let heartbeat = &heartbeats[0];
    assert_eq!(heartbeat.id.as_str(), "quorum:api");
    assert_eq!(heartbeat.interval.get().as_secs(), 30);
    assert_eq!(heartbeat.grace.get().as_secs(), 135);
    assert_eq!(heartbeat.component, None, "internal page only");
    assert_eq!(config.quorum_heartbeats("dmz-1"), []);
    assert_eq!(
        config.quorum_check_of("quorum:api").map(|c| c.id.as_str()),
        Some("api")
    );
    assert_eq!(config.quorum_check_of("node:api"), None);

    assert_eq!(config.alerts_behind("central"), Some(&[][..]));
    assert_eq!(
        config.alerts_behind("dmz-1"),
        Some(&[Id::parse("central").unwrap()][..])
    );
    assert_eq!(config.alerts_behind("elsewhere"), None);

    let one = run(&[("server.toml", &quorum_check("quorum = 1\n"))]).expect("valid");
    assert_eq!(one.quorum(&one.checks[0]), Some(1));
    let single = quorum_check("").replace("nodes = [\"central\", \"dmz-1\"]", "");
    let single = run(&[("server.toml", &single)]).expect("valid");
    assert_eq!(
        single.quorum(&single.checks[0]),
        None,
        "one node is no quorum"
    );
}

#[test]
fn a_quorum_is_checked_against_the_checks_nodes_and_where_they_push() {
    assert_eq!(
        errors(&[("q.toml", &quorum_check("quorum = 3\n"))]),
        ["q.toml:25:10: `quorum` must be between 1 and the check's 2 nodes"]
    );
    let single = quorum_check("quorum = 1\n").replace("nodes = [\"central\", \"dmz-1\"]", "");
    assert_eq!(
        errors(&[("q.toml", &single)]),
        ["q.toml:25:10: `quorum` needs a check run on two or more nodes, and `api` runs on one"]
    );
    let unpushed = quorum_check("")
        .replace("push_to = [\"central\"]\n", "")
        .replace("node_key = \"${ENV:NODE_KEY_B}\"\n", "");
    assert_eq!(
        errors(&[("q.toml", &unpushed)]),
        ["q.toml:22:9: no node receives the results of every node check `api` runs on"]
    );
    // Each node pushes to the other, so both derive it; with no
    // `alert_order` both would page.
    let both = quorum_check("")
        .replace(
            "url = \"https://central.example.com\"\n",
            "url = \"https://central.example.com\"\npush_to = [\"dmz-1\"]\n",
        )
        .replace(
            "\nid = \"dmz-1\"\n",
            "\nid = \"dmz-1\"\nurl = \"https://dmz-1.example.com\"\n",
        )
        .replace("alert_order = [\"central\", \"dmz-1\"]\n", "");
    assert_eq!(
        errors(&[("q.toml", &both)]),
        ["q.toml:25:9: check `api` is derived on `central`, `dmz-1`, and each would alert for it"]
    );
}
