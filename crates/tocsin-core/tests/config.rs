// SPDX-License-Identifier: AGPL-3.0-only

//! Loading configuration: every error class, located by file, line and
//! column.

use tocsin_core::config::{
    AuthMode, CheckType, Config, Diagnostic, Environment, Id, NotifierType, Secret, Source,
    TokenScope, load,
};

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
