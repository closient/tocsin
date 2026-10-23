// SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests of `tocsin config`, run against the built binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// The secrets `examples/config` reads, with values a test can look for.
const EXAMPLE_ENVIRONMENT: &[(&str, &str)] = &[
    (
        "TOCSIN_SMTP_URL",
        "smtp://status:SMTP-PASSWORD@mail.example.com:587",
    ),
    ("TOCSIN_CI_TOKEN", "CI-TOKEN-VALUE"),
    ("TOCSIN_BACKUP_TOKEN", "BACKUP-TOKEN-VALUE"),
    (
        "TOCSIN_SLACK_WEBHOOK_URL",
        "https://hooks.slack.com/services/SLACK-VALUE",
    ),
    ("TOCSIN_PUSHOVER_USER_KEY", "PUSHOVER-USER-VALUE"),
    ("TOCSIN_PUSHOVER_API_TOKEN", "PUSHOVER-TOKEN-VALUE"),
    ("TOCSIN_WEBHOOK_SIGNING_SECRET", "WEBHOOK-SIGNING-VALUE"),
];

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn tocsin(args: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tocsin"));
    command.args(args);
    for (name, _) in EXAMPLE_ENVIRONMENT {
        command.env_remove(name);
    }
    command.envs(environment.iter().copied());
    command.output().expect("tocsin runs")
}

/// Stdout must be exactly one JSON document.
fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout is a single JSON document")
}

fn write(folder: &Path, name: &str, text: &str) {
    let path = folder.join(name);
    std::fs::create_dir_all(path.parent().expect("has a parent")).expect("creates folders");
    std::fs::write(path, text).expect("writes");
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn the_examples_are_a_valid_configuration() {
    let examples = repository().join("examples/config");
    let output = tocsin(
        &[
            "--json",
            "config",
            "check",
            "--config",
            examples.to_str().unwrap(),
        ],
        EXAMPLE_ENVIRONMENT,
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stdout));
    let report = json_stdout(&output);
    assert_eq!(report["ok"], true);
    let files: Vec<String> = report["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| {
            let file = file.as_str().expect("a path");
            Path::new(file)
                .strip_prefix(&examples)
                .expect("under the folder")
                .display()
                .to_string()
        })
        .collect();
    assert_eq!(
        files,
        [
            "checks/infrastructure.toml",
            "checks/web.toml",
            "components.toml",
            "heartbeats.toml",
            "notifications.toml",
            "server.toml",
        ]
    );
    assert_eq!(report["counts"]["check"], 6);
    assert_eq!(report["counts"]["component"], 4);
}

#[test]
fn check_reports_every_error_as_one_json_document() {
    let folder = tempfile::tempdir().unwrap();
    write(
        folder.path(),
        "checks.toml",
        "[[check]]\nid = \"web\"\ncomponent = \"wbsite\"\ntype = \"http\"\ntarget = \"https://example.com\"\ninterval = \"soon\"\n",
    );
    write(
        folder.path(),
        "server.toml",
        "[server.email]\nfrom = \"noreply@example.com\"\nreply_to = \"support@example.com\"\n",
    );
    let output = tocsin(
        &[
            "config",
            "check",
            "--json",
            "--config",
            folder.path().to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    let document = json_stdout(&output);
    assert_eq!(document["error"]["kind"], "invalid_input");
    let errors = document["error"]["errors"].as_array().expect("errors");
    let located: Vec<String> = errors
        .iter()
        .map(|error| {
            let file = Path::new(error["file"].as_str().expect("file"));
            format!(
                "{}:{}:{}",
                file.file_name().unwrap().to_string_lossy(),
                error["line"],
                error["column"]
            )
        })
        .collect();
    // The bad duration fails the check outright, so its unknown component
    // is not reached; the sender is a separate item and still reported.
    assert_eq!(located, ["checks.toml:6:12", "server.toml:2:8"]);
    for error in errors {
        let keys: Vec<&str> = error
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["column", "file", "hint", "line", "message", "path"]);
    }
}

#[test]
fn check_lists_errors_as_text_on_stderr() {
    let folder = tempfile::tempdir().unwrap();
    write(
        folder.path(),
        "a.toml",
        "[[component]]\nid = \"api\"\nname = \"API\"\nlabel = \"x\"\n",
    );
    let output = tocsin(
        &[
            "config",
            "check",
            "--config",
            folder.path().to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    let file = folder.path().join("a.toml");
    assert!(
        stderr.contains(&format!("{}:4:1: unknown field `label`", file.display())),
        "{stderr}"
    );
}

#[test]
fn secrets_never_appear_in_output() {
    let examples = repository().join("examples/config");
    let output = tocsin(
        &[
            "--json",
            "config",
            "show",
            "--config",
            examples.to_str().unwrap(),
        ],
        EXAMPLE_ENVIRONMENT,
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let shown = text(&output.stdout);
    let config: Value = serde_json::from_str(&shown).expect("one document");
    assert_eq!(config["server"]["retention_days"], 90);
    assert_eq!(
        config["notifier"][0]["slack"]["webhook_url"],
        "<redacted from ${ENV:TOCSIN_SLACK_WEBHOOK_URL}>"
    );
    for (_, value) in EXAMPLE_ENVIRONMENT {
        assert!(!shown.contains(value), "{value} leaked into {shown}");
    }

    // A secret pasted into a file is rejected without being repeated.
    let folder = tempfile::tempdir().unwrap();
    write(
        folder.path(),
        "notify.toml",
        "[[notifier]]\nid = \"a\"\ntype = \"slack\"\n[notifier.slack]\nwebhook_url = \"https://hooks.slack.com/services/PASTED-VALUE\"\n",
    );
    for json in [true, false] {
        let mut args = vec![
            "config",
            "check",
            "--config",
            folder.path().to_str().unwrap(),
        ];
        if json {
            args.push("--json");
        }
        let output = tocsin(&args, &[]);
        assert_eq!(output.status.code(), Some(2));
        let everything = text(&output.stdout) + &text(&output.stderr);
        let location = if json {
            "\"line\":5"
        } else {
            "notify.toml:5:15"
        };
        assert!(everything.contains(location), "{everything}");
        assert!(!everything.contains("PASTED-VALUE"), "{everything}");
    }
}

#[test]
fn the_folder_walk_is_recursive_sorted_and_skips_hidden_entries() {
    let folder = tempfile::tempdir().unwrap();
    write(
        folder.path(),
        "b.toml",
        "[[component]]\nid = \"b\"\nname = \"B\"\n",
    );
    write(
        folder.path(),
        "a/z.toml",
        "[[component]]\nid = \"z\"\nname = \"Z\"\n",
    );
    write(folder.path(), "notes.txt", "not toml at all [");
    write(folder.path(), ".hidden.toml", "not toml at all [");
    write(folder.path(), ".git/config.toml", "not toml at all [");
    let elsewhere = tempfile::tempdir().unwrap();
    write(elsewhere.path(), "broken.toml", "not toml at all [");
    std::os::unix::fs::symlink(elsewhere.path(), folder.path().join("linked")).unwrap();
    std::os::unix::fs::symlink(folder.path().join("b.toml"), folder.path().join("c.toml")).unwrap();

    let output = tocsin(
        &[
            "--json",
            "config",
            "check",
            "--config",
            folder.path().to_str().unwrap(),
        ],
        &[],
    );
    // The symlinked file is read, so `b` is defined twice.
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stdout));
    let document = json_stdout(&output);
    let errors = document["error"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{document}");
    let message = errors[0]["message"].as_str().unwrap();
    assert!(
        message.starts_with("duplicate component id `b`"),
        "{message}"
    );
    assert!(errors[0]["file"].as_str().unwrap().ends_with("c.toml"));

    std::fs::remove_file(folder.path().join("c.toml")).unwrap();
    let output = tocsin(
        &[
            "--json",
            "config",
            "check",
            "--config",
            folder.path().to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stdout));
    let files = json_stdout(&output)["files"].clone();
    let names: Vec<String> = files
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            text(
                Path::new(file.as_str().unwrap())
                    .strip_prefix(folder.path())
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes(),
            )
        })
        .collect();
    assert_eq!(names, ["a/z.toml", "b.toml"]);
}

#[test]
fn check_rejects_a_missing_folder() {
    let output = tocsin(
        &[
            "--json",
            "config",
            "check",
            "--config",
            "/nonexistent/tocsin-config",
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json_stdout(&output)["error"]["kind"], "invalid_input");
}

#[test]
fn the_committed_schema_is_current() {
    let output = tocsin(&["config", "schema"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let committed = std::fs::read_to_string(repository().join("schema/tocsin.schema.json"))
        .expect("schema/tocsin.schema.json exists");
    assert!(
        text(&output.stdout) == committed,
        "schema/tocsin.schema.json is stale; regenerate it with\n  cargo run -- config schema > schema/tocsin.schema.json"
    );
    let schema = json_stdout(&tocsin(&["--json", "config", "schema"], &[]));
    assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
    assert_eq!(schema["title"], "tocsin configuration file");
}
