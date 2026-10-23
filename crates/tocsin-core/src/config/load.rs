// SPDX-License-Identifier: AGPL-3.0-only

//! Parsing, merging and validating a folder's worth of configuration files.
//!
//! Every item (`[server]`, each `[[check]]`, ...) is deserialized on its
//! own, so one malformed item does not hide problems in the others. Items
//! are then merged and the rules that span files are checked. Every problem
//! found is reported, each located by file, line and column.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::net::IpAddr;
use std::ops::Range;

use serde::de::DeserializeOwned;
use toml::Spanned;
use toml::de::{DeTable, DeValue, ValueDeserializer};

use super::assertion::{Condition, TextAssertion};
use super::check::CheckType;
use super::diagnostic::{Diagnostic, LineIndex};
use super::notifier::NotifierType;
use super::values::{Id, Secret, Text, email_address, is_unanswerable};
use super::{
    Check, Component, Config, Group, Heartbeat, Link, Notifier, Page, Project, Route, Server, Token,
};

/// One configuration file's name and contents.
#[derive(Debug, Clone)]
pub struct Source {
    /// How diagnostics name the file, such as `config/checks.toml`.
    pub name: String,
    /// The file's text, or why it could not be read, such as "the file is
    /// not UTF-8 text".
    pub text: Result<String, String>,
}

/// What loading needs from the machine it runs on, passed in so this crate
/// does no I/O.
pub struct Environment<'a> {
    /// The default `node_id`.
    pub hostname: String,
    /// Looks up an environment variable, for `${ENV:NAME}` secrets.
    pub variable: &'a dyn Fn(&str) -> Option<String>,
}

/// The top-level keys a file may hold, in the order they are processed.
pub(crate) const TOP_LEVEL_KEYS: &[&str] = &[
    "server",
    "page",
    "group",
    "component",
    "check",
    "heartbeat",
    "project",
    "notifier",
    "route",
    "token",
];

/// Parses, merges and validates configuration files, given in merge order.
///
/// # Errors
///
/// Returns every problem found, ordered by file, line and column.
pub fn load(sources: &[Source], environment: &Environment<'_>) -> Result<Config, Vec<Diagnostic>> {
    let mut loader = Loader::default();
    for (index, source) in sources.iter().enumerate() {
        loader.parse(index, source);
    }
    let mut items = loader.collect();
    loader.check_references(&items);
    loader.validate(&mut items);
    loader.resolve_secrets(&mut items, environment);
    loader.into_result(items, environment)
}

/// A position in a file's data: `check[2].http.method`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct Path(Vec<Step>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Step {
    Key(String),
    Index(usize),
}

impl Path {
    fn key(&self, key: &str) -> Path {
        let mut steps = self.0.clone();
        steps.push(Step::Key(key.to_owned()));
        Path(steps)
    }

    fn keys(&self, keys: &[&str]) -> Path {
        keys.iter().fold(self.clone(), |path, key| path.key(key))
    }

    fn index(&self, index: usize) -> Path {
        let mut steps = self.0.clone();
        steps.push(Step::Index(index));
        Path(steps)
    }
}

impl fmt::Display for Path {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (position, step) in self.0.iter().enumerate() {
            match step {
                Step::Key(key) if position == 0 => formatter.write_str(key)?,
                Step::Key(key) => write!(formatter, ".{key}")?,
                Step::Index(index) => write!(formatter, "[{index}]")?,
            }
        }
        Ok(())
    }
}

/// Where an item or field came from: a parsed file and a path inside it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Origin {
    file: usize,
    path: Path,
}

impl Origin {
    fn key(&self, key: &str) -> Origin {
        Origin {
            file: self.file,
            path: self.path.key(key),
        }
    }

    fn keys(&self, keys: &[&str]) -> Origin {
        Origin {
            file: self.file,
            path: self.path.keys(keys),
        }
    }

    fn index(&self, index: usize) -> Origin {
        Origin {
            file: self.file,
            path: self.path.index(index),
        }
    }
}

/// A deserialized item and where it came from.
struct Item<T> {
    origin: Origin,
    value: T,
}

/// Every item that deserialized, by kind.
#[derive(Default)]
struct Items {
    server: Option<Item<Server>>,
    page: Option<Item<Page>>,
    groups: Vec<Item<Group>>,
    components: Vec<Item<Component>>,
    checks: Vec<Item<Check>>,
    heartbeats: Vec<Item<Heartbeat>>,
    projects: Vec<Item<Project>>,
    notifiers: Vec<Item<Notifier>>,
    routes: Vec<Item<Route>>,
    tokens: Vec<Item<Token>>,
}

struct ParsedFile<'a> {
    source: usize,
    name: &'a str,
    text: &'a str,
    root: DeTable<'a>,
    lines: LineIndex,
}

#[derive(Default)]
struct Loader<'a> {
    files: Vec<ParsedFile<'a>>,
    /// Each diagnostic with the index of its source, for ordering.
    diagnostics: Vec<(Option<usize>, Diagnostic)>,
    /// Every id written in a file, by kind, with where it was first written.
    /// Read from the raw tables, so an item that failed to deserialize
    /// still counts and its references are not reported as unknown.
    ids: HashMap<&'static str, BTreeMap<String, Origin>>,
    /// Where each singleton table was first defined.
    singletons: HashMap<&'static str, Origin>,
    /// Every item that deserialized, of every kind.
    deserialized: Vec<Origin>,
    /// Whether a file could not be read or parsed, so ids it defines are
    /// unknown and references to them cannot be judged.
    incomplete: bool,
}

impl<'a> Loader<'a> {
    fn parse(&mut self, source: usize, file: &'a Source) {
        let text = match &file.text {
            Ok(text) => text,
            Err(reason) => {
                self.incomplete = true;
                self.diagnostics.push((
                    Some(source),
                    Diagnostic::in_file(file.name.clone(), reason.clone()),
                ));
                return;
            }
        };
        let (root, errors) = DeTable::parse_recoverable(text);
        let lines = LineIndex::new(text);
        if errors.is_empty() {
            self.files.push(ParsedFile {
                source,
                name: &file.name,
                text,
                root: root.into_inner(),
                lines,
            });
            return;
        }
        // The partial table is not trusted: an id on or after the broken
        // line may be missing from it.
        self.incomplete = true;
        // Recovery reports cascades of errors from one mistake; the first on
        // each line is the one worth reading.
        let mut lines_seen = HashSet::new();
        for error in errors {
            let position = error.span().map(|span| lines.locate(text, span.start));
            if position.is_some_and(|(line, _)| !lines_seen.insert(line)) {
                continue;
            }
            self.diagnostics.push((
                Some(source),
                Diagnostic {
                    file: Some(file.name.clone()),
                    line: position.map(|(line, _)| line),
                    column: position.map(|(_, column)| column),
                    path: None,
                    message: format!("TOML syntax error: {}", error.message()),
                    hint: None,
                },
            ));
        }
    }

    // ---- locating -------------------------------------------------------

    /// The span of the deepest part of `path` that exists in the file.
    fn span(&self, origin: &Origin) -> Option<Range<usize>> {
        let mut table = Some(&self.files[origin.file].root);
        let mut array = None;
        let mut found = None;
        for step in &origin.path.0 {
            let next = match step {
                Step::Key(key) => table.and_then(|table| table.get(key.as_str())),
                Step::Index(index) => {
                    array.and_then(|array: &[Spanned<DeValue<'_>>]| array.get(*index))
                }
            };
            let Some(next) = next else { break };
            found = Some(next.span());
            table = next.get_ref().as_table();
            array = next.get_ref().as_array().map(|array| &array[..]);
        }
        found
    }

    fn position(&self, file: usize, span: Option<Range<usize>>) -> Option<(usize, usize)> {
        let parsed = &self.files[file];
        span.map(|span| parsed.lines.locate(parsed.text, span.start))
    }

    /// `file:line:column` of an origin, for messages that name a second
    /// location.
    fn describe(&self, origin: &Origin) -> String {
        let name = self.files[origin.file].name;
        match self.position(origin.file, self.span(origin)) {
            Some((line, column)) => format!("{name}:{line}:{column}"),
            None => name.to_owned(),
        }
    }

    fn report_at(
        &mut self,
        file: usize,
        span: Option<Range<usize>>,
        path: &Path,
        message: String,
        hint: Option<String>,
    ) {
        let position = self.position(file, span);
        let parsed = &self.files[file];
        self.diagnostics.push((
            Some(parsed.source),
            Diagnostic {
                file: Some(parsed.name.to_owned()),
                line: position.map(|(line, _)| line),
                column: position.map(|(_, column)| column),
                path: (!path.0.is_empty()).then(|| path.to_string()),
                message,
                hint,
            },
        ));
    }

    fn report(&mut self, origin: &Origin, message: impl Into<String>, hint: Option<String>) {
        let span = self.span(origin);
        self.report_at(origin.file, span, &origin.path, message.into(), hint);
    }

    // ---- collecting -----------------------------------------------------

    /// Deserializes every item of every parsed file.
    fn collect(&mut self) -> Items {
        let mut items = Items::default();
        for file in 0..self.files.len() {
            let mut keys: Vec<(String, Range<usize>)> = self.files[file]
                .root
                .keys()
                .map(|key| (key.get_ref().to_string(), key.span()))
                .collect();
            keys.sort_by_key(|(_, span)| span.start);
            for (key, span) in keys {
                let origin = Origin {
                    file,
                    path: Path::default().key(&key),
                };
                match key.as_str() {
                    "server" => {
                        items.server = self.singleton(&origin, "server", items.server.take());
                    }
                    "page" => items.page = self.singleton(&origin, "page", items.page.take()),
                    "group" => items.groups.extend(self.array(&origin, "group")),
                    "component" => items.components.extend(self.array(&origin, "component")),
                    "check" => items.checks.extend(self.array(&origin, "check")),
                    "heartbeat" => items.heartbeats.extend(self.array(&origin, "heartbeat")),
                    "project" => items.projects.extend(self.array(&origin, "project")),
                    "notifier" => items.notifiers.extend(self.array(&origin, "notifier")),
                    "route" => items.routes.extend(self.array(&origin, "route")),
                    "token" => items.tokens.extend(self.array(&origin, "token")),
                    _ => {
                        let hint = closest(&key, TOP_LEVEL_KEYS.iter().copied()).map_or_else(
                            || format!("top-level tables are {}", list(TOP_LEVEL_KEYS)),
                            |near| format!("did you mean `{near}`?"),
                        );
                        self.report_at(
                            file,
                            Some(span),
                            &origin.path,
                            format!("unknown table `{key}`"),
                            Some(hint),
                        );
                    }
                }
            }
        }
        items
    }

    /// Deserializes `[server]` or `[page]`, which only one file may hold.
    fn singleton<T: DeserializeOwned>(
        &mut self,
        origin: &Origin,
        kind: &'static str,
        existing: Option<Item<T>>,
    ) -> Option<Item<T>> {
        if let Some(first) = self.singletons.get(kind).cloned() {
            let message = format!("`[{kind}]` is already defined at {}", self.describe(&first));
            self.report(
                origin,
                message,
                Some(format!("keep `[{kind}]` in one file")),
            );
            return existing;
        }
        self.singletons.insert(kind, origin.clone());
        let value = self.raw(origin)?.clone();
        if value.get_ref().as_table().is_none() {
            self.report(
                origin,
                format!("`{kind}` must be a table: write `[{kind}]`"),
                None,
            );
            return existing;
        }
        let value = self.deserialize(origin, &value)?;
        self.deserialized.push(origin.clone());
        Some(Item {
            origin: origin.clone(),
            value,
        })
    }

    /// Deserializes each table of a `[[kind]]` array, registering its id.
    fn array<T: DeserializeOwned>(&mut self, origin: &Origin, kind: &'static str) -> Vec<Item<T>> {
        let Some(value) = self.raw(origin).cloned() else {
            return Vec::new();
        };
        let Some(elements) = value.get_ref().as_array() else {
            self.report(
                origin,
                format!("`{kind}` must be an array of tables: write `[[{kind}]]`"),
                None,
            );
            return Vec::new();
        };
        let mut items = Vec::new();
        for (index, element) in elements.iter().enumerate() {
            let origin = origin.index(index);
            let Some(table) = element.get_ref().as_table() else {
                self.report(&origin, format!("each `{kind}` must be a table"), None);
                continue;
            };
            if let Some(id) = table.get("id").and_then(|id| id.get_ref().as_str()) {
                self.register_id(kind, id, origin.key("id"));
            }
            if let Some(value) = self.deserialize(&origin, element) {
                self.deserialized.push(origin.clone());
                items.push(Item { origin, value });
            }
        }
        items
    }

    fn register_id(&mut self, kind: &'static str, id: &str, origin: Origin) {
        if let Some(first) = self.ids.get(kind).and_then(|ids| ids.get(id)).cloned() {
            let message = format!(
                "duplicate {kind} id `{id}`; first defined at {}",
                self.describe(&first)
            );
            self.report(
                &origin,
                message,
                Some(format!(
                    "ids must be unique among {kind}s across every file"
                )),
            );
        } else {
            self.ids
                .entry(kind)
                .or_default()
                .insert(id.to_owned(), origin);
        }
    }

    fn raw(&self, origin: &Origin) -> Option<&Spanned<DeValue<'a>>> {
        let mut steps = origin.path.0.iter();
        let Some(Step::Key(first)) = steps.next() else {
            return None;
        };
        let mut value = self.files[origin.file].root.get(first.as_str())?;
        for step in steps {
            value = match step {
                Step::Key(key) => value.get_ref().as_table()?.get(key.as_str())?,
                Step::Index(index) => value.get_ref().as_array()?.get(*index)?,
            };
        }
        Some(value)
    }

    fn deserialize<T: DeserializeOwned>(
        &mut self,
        origin: &Origin,
        value: &Spanned<DeValue<'a>>,
    ) -> Option<T> {
        match T::deserialize(ValueDeserializer::from(value.clone())) {
            Ok(item) => Some(item),
            Err(error) => {
                let span = error.span().unwrap_or_else(|| value.span());
                let mut path = origin.path.clone();
                path.0.extend(steps_to(value, &span).unwrap_or_default());
                let message = error.message().to_owned();
                let hint = hint_for(&message);
                self.report_at(origin.file, Some(span), &path, message, hint);
                None
            }
        }
    }

    // ---- references -----------------------------------------------------

    fn check_references(&mut self, items: &Items) {
        // An id may sit in a file that could not be read; reporting every
        // reference to it as unknown would bury the real error.
        if self.incomplete {
            return;
        }
        let mut references: Vec<(&'static str, &Id, Origin)> = Vec::new();
        for component in &items.components {
            if let Some(group) = &component.value.group {
                references.push(("group", group, component.origin.key("group")));
            }
        }
        for check in &items.checks {
            if let Some(component) = &check.value.component {
                references.push(("component", component, check.origin.key("component")));
            }
        }
        for heartbeat in &items.heartbeats {
            if let Some(component) = &heartbeat.value.component {
                references.push(("component", component, heartbeat.origin.key("component")));
            }
        }
        for project in &items.projects {
            if let Some(component) = &project.value.component {
                references.push(("component", component, project.origin.key("component")));
            }
        }
        for route in &items.routes {
            let lists = [
                ("notifier", "notifiers", &route.value.notifiers),
                ("component", "components", &route.value.components),
                ("group", "groups", &route.value.groups),
                ("check", "checks", &route.value.checks),
                ("heartbeat", "heartbeats", &route.value.heartbeats),
            ];
            for (kind, key, ids) in lists {
                for (index, id) in ids.iter().enumerate() {
                    references.push((kind, id, route.origin.key(key).index(index)));
                }
            }
        }
        for (kind, id, origin) in references {
            let known = self.ids.get(kind);
            if known.is_some_and(|known| known.contains_key(id.as_str())) {
                continue;
            }
            let hint = match known.filter(|known| !known.is_empty()) {
                None => format!("no `[[{kind}]]` is defined in any file"),
                Some(known) => closest(id.as_str(), known.keys().map(String::as_str)).map_or_else(
                    || format!("defined {kind}s: {}", list(known.keys())),
                    |near| format!("did you mean `{near}`?"),
                ),
            };
            self.report(&origin, format!("unknown {kind} `{id}`"), Some(hint));
        }
    }

    // ---- validation -----------------------------------------------------

    fn validate(&mut self, items: &mut Items) {
        if let Some(server) = &items.server {
            self.validate_server(server);
        }
        self.validate_email_senders(items);
        if let Some(page) = &items.page
            && let Some(url) = &page.value.homepage_url
        {
            self.url(&page.origin.key("homepage_url"), url);
        }
        for project in &items.projects {
            if let Some(url) = &project.value.url {
                self.url(&project.origin.key("url"), url);
            }
        }
        for check in &mut items.checks {
            self.validate_check(check);
            self.validate_links(&check.origin, &check.value.links);
        }
        for heartbeat in &items.heartbeats {
            self.validate_links(&heartbeat.origin, &heartbeat.value.links);
            if heartbeat.value.interval.get() < std::time::Duration::from_secs(1) {
                self.report(
                    &heartbeat.origin.key("interval"),
                    "a heartbeat interval must be at least 1s",
                    None,
                );
            }
        }
        for notifier in &items.notifiers {
            self.validate_notifier(notifier);
        }
        for route in &items.routes {
            if route.value.notifiers.is_empty() {
                self.report(
                    &route.origin.key("notifiers"),
                    "a route needs at least one notifier",
                    None,
                );
            }
        }
        for token in &items.tokens {
            if token.value.scopes.is_empty() {
                self.report(
                    &token.origin.key("scopes"),
                    "a token needs at least one scope",
                    Some("scopes are heartbeat, incident and release".to_owned()),
                );
            }
        }
    }

    fn validate_server(&mut self, server: &Item<Server>) {
        let origin = &server.origin;
        let value = &server.value;
        if let Some(url) = &value.public_url {
            self.url(&origin.key("public_url"), url);
        }
        if let Some(node_id) = &value.node_id
            && (node_id.is_empty() || node_id.contains(char::is_whitespace))
        {
            self.report(
                &origin.key("node_id"),
                "`node_id` must be non-empty with no whitespace",
                None,
            );
        }
        if value.retention_days == 0 {
            self.report(
                &origin.key("retention_days"),
                "`retention_days` must be at least 1",
                None,
            );
        }
        if value.max_concurrent_checks == 0 {
            self.report(
                &origin.key("max_concurrent_checks"),
                "`max_concurrent_checks` must be at least 1",
                None,
            );
        }
        let Some(email) = &value.email else { return };
        for (key, address) in [("from", &email.from), ("reply_to", &email.reply_to)] {
            let Some(address) = address else { continue };
            let origin = origin.keys(&["email", key]);
            match email_address(address) {
                Err(message) => self.report(&origin, message, None),
                Ok(bare) if is_unanswerable(bare) => self.report(
                    &origin,
                    format!(
                        "`{bare}` is a no-reply address; every email tocsin sends must be one a person can answer"
                    ),
                    Some(format!(
                        "use a mailbox someone reads, such as support@{}",
                        bare.rsplit('@').next().unwrap_or_default()
                    )),
                ),
                Ok(_) => {}
            }
        }
    }

    /// `[server.email]` is required, complete, once any notifier sends email.
    fn validate_email_senders(&mut self, items: &Items) {
        let Some(first) = items
            .notifiers
            .iter()
            .find(|notifier| notifier.value.notifier_type == NotifierType::Email)
        else {
            return;
        };
        let id = first.value.id.clone();
        let email = items
            .server
            .as_ref()
            .and_then(|server| server.value.email.as_ref().map(|email| (server, email)));
        let Some((server, email)) = email else {
            // `[server]` that failed to deserialize, or a file that could
            // not be read, may hold `[server.email]`; its error is reported.
            if items.server.is_none() && (self.singletons.contains_key("server") || self.incomplete)
            {
                return;
            }
            self.report(
                &first.origin.key("type"),
                format!("notifier `{id}` sends email, but no file sets `[server.email]`"),
                Some("add `[server.email]` with `from`, `reply_to` and `smtp_url`".to_owned()),
            );
            return;
        };
        let missing = [
            ("from", email.from.is_none()),
            ("reply_to", email.reply_to.is_none()),
            ("smtp_url", email.smtp_url.is_none()),
        ];
        for (key, absent) in missing {
            if absent {
                self.report(
                    &server.origin.key("email"),
                    format!("`[server.email]` needs `{key}` because notifier `{id}` sends email"),
                    None,
                );
            }
        }
    }

    fn validate_check(&mut self, check: &mut Item<Check>) {
        let origin = check.origin.clone();
        let value = &mut check.value;
        let check_type = value.check_type;
        if let Err(message) = target(check_type, &value.target) {
            self.report(
                &origin.key("target"),
                message,
                Some(target_example(check_type)),
            );
        }
        if value.interval.get() < std::time::Duration::from_secs(1) {
            self.report(
                &origin.key("interval"),
                "`interval` must be at least 1s",
                None,
            );
        }
        if value.timeout.get().is_zero() {
            self.report(
                &origin.key("timeout"),
                "`timeout` must be more than 0s",
                None,
            );
        } else if value.timeout >= value.interval {
            self.report(
                &origin.key("timeout"),
                format!(
                    "`timeout` ({}) must be shorter than `interval` ({})",
                    value.timeout, value.interval
                ),
                None,
            );
        }
        if value.confirm == 0 {
            self.report(&origin.key("confirm"), "`confirm` must be at least 1", None);
        }
        let tables = [
            ("http", CheckType::Http, value.http.is_some()),
            ("dns", CheckType::Dns, value.dns.is_some()),
            ("tls", CheckType::Tls, value.tls.is_some()),
            ("tcp", CheckType::Tcp, value.tcp.is_some()),
        ];
        for (table, applies_to, present) in tables {
            if present && applies_to != check_type {
                self.report(
                    &origin.key(table),
                    format!(
                        "`[check.{table}]` only applies to {table} checks, and this check is `{}`",
                        check_type.as_str()
                    ),
                    None,
                );
            }
        }
        self.validate_settings(&origin, value);
        // Settings tables are optional; fill in their defaults.
        match check_type {
            CheckType::Http => {
                value.http.get_or_insert_with(Default::default);
            }
            CheckType::Dns => {
                value.dns.get_or_insert_with(Default::default);
            }
            CheckType::Tls => {
                value.tls.get_or_insert_with(Default::default);
            }
            CheckType::Tcp => {
                value.tcp.get_or_insert_with(Default::default);
            }
            CheckType::Icmp => {}
        }
        self.validate_assertions(&origin, value);
    }

    /// Each `[[check.assert]]`: one condition, valid for the check.
    fn validate_assertions(&mut self, origin: &Origin, value: &Check) {
        let check_type = value.check_type;
        let plain_http = check_type == CheckType::Http
            && value
                .target
                .get(..7)
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"));
        for (index, assertion) in value.assertions.iter().enumerate() {
            let origin = origin.key("assert").index(index);
            match assertion.conditions().as_slice() {
                [] => self.report(
                    &origin,
                    "an assertion needs a condition",
                    Some(format!("use one of {}", list(super::CONDITION_KEYS))),
                ),
                [condition] if plain_http && condition.needs_certificate() => self.report(
                    &origin.key(condition.key()),
                    format!(
                        "a `{}` assertion needs an https:// target, and this one is http://",
                        condition.key()
                    ),
                    None,
                ),
                [condition] => self.validate_condition(&origin, check_type, condition),
                several => self.report(
                    &origin,
                    format!(
                        "an assertion holds one condition, and this one has {}",
                        list(several.iter().map(Condition::key))
                    ),
                    Some("split them into separate `[[check.assert]]` tables".to_owned()),
                ),
            }
        }
    }

    /// `[check.dns] resolver`, the `server_name`s and HTTP authentication.
    fn validate_settings(&mut self, origin: &Origin, value: &Check) {
        if let Some(resolver) = value.dns.as_ref().and_then(|dns| dns.resolver.as_ref())
            && host_and_port(resolver).is_none_or(|(host, _)| host.parse::<IpAddr>().is_err())
        {
            self.report(
                &origin.keys(&["dns", "resolver"]),
                format!("`{resolver}` is not an IP address or ip:port"),
                Some(
                    "such as \"1.1.1.1\", \"192.0.2.53:5353\" or \"[2001:db8::53]:53\"".to_owned(),
                ),
            );
        }
        let server_names = [
            (
                "tls",
                value.tls.as_ref().and_then(|tls| tls.server_name.as_ref()),
            ),
            (
                "http",
                value
                    .http
                    .as_ref()
                    .and_then(|http| http.server_name.as_ref()),
            ),
        ];
        for (table, name) in server_names {
            if let Some(name) = name
                && !is_host(name)
            {
                self.report(
                    &origin.keys(&[table, "server_name"]),
                    format!("`{name}` is not a hostname"),
                    None,
                );
            }
        }
        if let Some(http) = &value.http
            && http.bearer_token.is_some()
            && http.basic_auth.is_some()
        {
            self.report(
                &origin.keys(&["http", "basic_auth"]),
                "give `bearer_token` or `basic_auth`, not both",
                None,
            );
        }
    }

    fn validate_condition(
        &mut self,
        origin: &Origin,
        check_type: CheckType,
        condition: &Condition<'_>,
    ) {
        let origin = origin.key(condition.key());
        if !condition.applies_to(check_type) {
            self.report(
                &origin,
                format!(
                    "a `{}` assertion does not apply to {} checks",
                    condition.key(),
                    check_type.as_str()
                ),
                None,
            );
            return;
        }
        let one_matcher = |count: usize, names: &str| {
            (count != 1).then(|| (format!("give exactly one of {names}"), None::<String>))
        };
        let problem = match condition {
            Condition::Status([]) => Some(("list at least one status code".to_owned(), None)),
            Condition::Status(codes) => codes
                .iter()
                .find(|code| !(100..=599).contains(*code))
                .map(|code| (format!("{code} is not an HTTP status code"), None)),
            Condition::LatencyMilliseconds(comparison)
            | Condition::CertificateDaysRemaining(comparison) => comparison.is_empty().then(|| {
                (
                    "give at least one bound: lt, lte, gt, gte or eq".to_owned(),
                    None,
                )
            }),
            Condition::Json(json) => match crate::probe::json_path(&json.path) {
                Err(message) => Some((
                    message,
                    Some(bracketed(&json.path).map_or_else(
                        || {
                            "such as \"$.status\", \"$.checks[0].ok\" or \"$.components[?@.name=='API'].status\""
                                .to_owned()
                        },
                        |path| format!("use bracket notation: {path}"),
                    )),
                )),
                Ok(_) => one_matcher(json.matcher_count(), "eq, ne, exists or contains"),
            },
            Condition::Body(text) | Condition::Banner(text) => text_problem(text),
            Condition::HostnameValid(_) | Condition::ChainValid(_) => None,
            Condition::Header(header) if header.name.is_empty() => {
                Some(("a header assertion needs a `name`".to_owned(), None))
            }
            Condition::Header(header) => {
                one_matcher(header.matcher_count(), "eq, contains or exists")
            }
            Condition::Records(records) => {
                one_matcher(records.matcher_count(), "contains or eq").or_else(|| {
                    let empty = records.contains.as_ref().is_some_and(String::is_empty)
                        || records.eq.as_ref().is_some_and(|eq| {
                            eq.is_empty() || eq.iter().any(String::is_empty)
                        });
                    empty.then(|| ("records must not be empty".to_owned(), None))
                })
            }
        };
        if let Some((message, hint)) = problem {
            self.report(&origin, message, hint);
        }
    }

    fn validate_notifier(&mut self, notifier: &Item<Notifier>) {
        let origin = &notifier.origin;
        let value = &notifier.value;
        let wanted = value.notifier_type;
        let present = value.settings_present();
        if !present.contains(&wanted) {
            let name = wanted.as_str();
            self.report(
                &origin.key("type"),
                format!("a `{name}` notifier needs a `[notifier.{name}]` table"),
                None,
            );
        }
        for other in present.into_iter().filter(|other| *other != wanted) {
            self.report(
                &origin.key(other.as_str()),
                format!(
                    "`[notifier.{}]` does not apply to a `{}` notifier",
                    other.as_str(),
                    wanted.as_str()
                ),
                None,
            );
        }
        if let Some(email) = &value.email {
            if email.to.is_empty() {
                self.report(
                    &origin.keys(&["email", "to"]),
                    "list at least one recipient",
                    None,
                );
            }
            for (index, address) in email.to.iter().enumerate() {
                if let Err(message) = email_address(address) {
                    self.report(&origin.keys(&["email", "to"]).index(index), message, None);
                }
            }
        }
        let urls = [
            ("ntfy", value.ntfy.as_ref().map(|ntfy| &ntfy.url)),
            (
                "webhook",
                value.webhook.as_ref().map(|webhook| &webhook.url),
            ),
            (
                "alertmanager",
                value.alertmanager.as_ref().map(|manager| &manager.url),
            ),
        ];
        for (table, url) in urls {
            if let Some(url) = url {
                self.url(&origin.keys(&[table, "url"]), url);
            }
        }
        if let Some(telegram) = &value.telegram
            && telegram.chat_id.is_empty()
        {
            self.report(
                &origin.keys(&["telegram", "chat_id"]),
                "`chat_id` must not be empty",
                None,
            );
        }
    }

    fn validate_links(&mut self, origin: &Origin, links: &[Link]) {
        for (index, link) in links.iter().enumerate() {
            let origin = origin.key("link").index(index);
            if link.name.trim().is_empty() {
                self.report(&origin.key("name"), "a link needs a non-empty `name`", None);
            }
            self.url(&origin.key("url"), &link.url);
        }
    }

    fn url(&mut self, origin: &Origin, url: &str) {
        if let Err(message) = http_url(url) {
            self.report(origin, message, None);
        }
    }

    // ---- secrets --------------------------------------------------------

    /// Rejects an `http` check's header that could not be sent: a name that
    /// is not an HTTP token, or a value (secrets included, once resolved)
    /// holding a control character such as a newline. A secret's value is
    /// never repeated; its variable is named instead.
    fn validate_headers(&mut self, items: &Items) {
        for check in &items.checks {
            let Some(http) = &check.value.http else {
                continue;
            };
            for (name, value) in &http.headers {
                let origin = check.origin.keys(&["http", "headers", name]);
                if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
                    self.report(
                        &origin,
                        format!("`{name}` is not an HTTP header name"),
                        Some(
                            "a header name is letters, digits and !#$%&'*+-.^_`|~, such as \"X-Request-Id\""
                                .to_owned(),
                        ),
                    );
                }
                let (text, source) = match value {
                    Text::Literal(text) => (Some(text.as_str()), None),
                    Text::Secret(secret) => (secret.resolved(), Some(secret.variable())),
                };
                if let Some(text) = text {
                    self.header_value(&origin, text, source);
                }
            }
            if let Some(token) = &http.bearer_token
                && let Some(text) = token.resolved()
            {
                self.header_value(
                    &check.origin.keys(&["http", "bearer_token"]),
                    &format!("Bearer {text}"),
                    Some(token.variable()),
                );
            }
        }
    }

    /// Reports `text` when it cannot be an HTTP header value, naming the
    /// environment variable it came from rather than repeating it.
    fn header_value(&mut self, origin: &Origin, text: &str, variable: Option<&str>) {
        if http::HeaderValue::from_str(text).is_ok() {
            return;
        }
        let message = match variable {
            Some(variable) => format!(
                "the value of environment variable `{variable}` cannot be sent in an HTTP header: it holds a control character, such as a newline, or a character outside ASCII"
            ),
            None => "this value cannot be sent in an HTTP header: it holds a control character, such as a newline, or a character outside ASCII".to_owned(),
        };
        self.report(origin, message, None);
    }

    /// Resolves every secret from the environment, then rejects `${ENV:...}`
    /// anywhere that is not a secret field.
    fn resolve_secrets(&mut self, items: &mut Items, environment: &Environment<'_>) {
        let mut secrets: Vec<(Origin, &mut Secret)> = Vec::new();
        if let Some(server) = &mut items.server
            && let Some(smtp) = server
                .value
                .email
                .as_mut()
                .and_then(|email| email.smtp_url.as_mut())
        {
            secrets.push((server.origin.keys(&["email", "smtp_url"]), smtp));
        }
        for notifier in &mut items.notifiers {
            for (path, secret) in notifier.value.secrets_mut() {
                secrets.push((notifier.origin.keys(&path), secret));
            }
        }
        for check in &mut items.checks {
            let Some(http) = &mut check.value.http else {
                continue;
            };
            if let Some(secret) = &mut http.bearer_token {
                secrets.push((check.origin.keys(&["http", "bearer_token"]), secret));
            }
            if let Some(basic) = &mut http.basic_auth {
                if let Text::Secret(secret) = &mut basic.username {
                    secrets.push((
                        check.origin.keys(&["http", "basic_auth", "username"]),
                        secret,
                    ));
                }
                secrets.push((
                    check.origin.keys(&["http", "basic_auth", "password"]),
                    &mut basic.password,
                ));
            }
            for (name, header) in &mut http.headers {
                if let Text::Secret(secret) = header {
                    secrets.push((check.origin.keys(&["http", "headers", name]), secret));
                }
            }
            if let Some(Text::Secret(secret)) = &mut http.body {
                secrets.push((check.origin.keys(&["http", "body"]), secret));
            }
        }
        for token in &mut items.tokens {
            secrets.push((token.origin.key("secret"), &mut token.value.secret));
        }

        let mut secret_fields = HashSet::new();
        for (origin, secret) in secrets {
            let variable = secret.variable().to_owned();
            match (environment.variable)(&variable) {
                Some(value) if !value.is_empty() => secret.resolve(value),
                found => {
                    let state = if found.is_some() { "empty" } else { "not set" };
                    self.report(
                        &origin,
                        format!("environment variable `{variable}` is {state}"),
                        Some(format!("set {variable} in the environment tocsin runs in")),
                    );
                }
            }
            secret_fields.insert(origin);
        }

        self.validate_smtp_url(items);
        self.reject_shared_token_secrets(items);
        self.validate_headers(items);

        // Items that failed to deserialize are skipped: their secret fields
        // are unknown, and their error is already reported.
        for origin in self.deserialized.clone() {
            let Some(value) = self.raw(&origin).cloned() else {
                continue;
            };
            let mut found = Vec::new();
            interpolations(&value, &origin, &mut found);
            for field in found {
                if !secret_fields.contains(&field) {
                    self.report(
                        &field,
                        "`${ENV:...}` is only read in secret fields, and this field is not one",
                        Some(
                            "write the value itself; docs/config.md marks which fields are secret"
                                .to_owned(),
                        ),
                    );
                }
            }
        }
    }

    /// The resolved `smtp_url` must be an SMTP URL. It is a secret, so the
    /// message never repeats it.
    fn validate_smtp_url(&mut self, items: &Items) {
        let Some(server) = &items.server else { return };
        let Some(smtp) = server
            .value
            .email
            .as_ref()
            .and_then(|email| email.smtp_url.as_ref())
        else {
            return;
        };
        let Some(url) = smtp.resolved() else { return };
        let scheme = url.split_once("://").map(|(scheme, _)| scheme);
        if !matches!(scheme, Some("smtp" | "smtps")) {
            self.report(
                &server.origin.keys(&["email", "smtp_url"]),
                format!(
                    "the value of `{}` is not an smtp:// or smtps:// URL",
                    smtp.variable()
                ),
                Some("such as smtps://user:password@mail.example.com:465".to_owned()),
            );
        }
    }

    /// Each token needs its own secret, or a request cannot be traced to
    /// one token and revoking one revokes both. The message names the
    /// tokens, never the value.
    fn reject_shared_token_secrets(&mut self, items: &Items) {
        let mut seen: HashMap<&str, &Item<Token>> = HashMap::new();
        let mut shared = Vec::new();
        for token in &items.tokens {
            let Some(value) = token.value.secret.resolved() else {
                continue;
            };
            match seen.get(value) {
                Some(first) => shared.push((*first, token)),
                None => {
                    seen.insert(value, token);
                }
            }
        }
        for (first, token) in shared {
            let message = format!(
                "token `{}` has the same secret as token `{}` at {}",
                token.value.id,
                first.value.id,
                self.describe(&first.origin.key("secret"))
            );
            self.report(
                &token.origin.key("secret"),
                message,
                Some("give each token its own secret".to_owned()),
            );
        }
    }

    // ---- result ---------------------------------------------------------

    fn into_result(
        mut self,
        items: Items,
        environment: &Environment<'_>,
    ) -> Result<Config, Vec<Diagnostic>> {
        if !self.diagnostics.is_empty() {
            self.diagnostics.sort_by_key(|(source, diagnostic)| {
                (
                    source.unwrap_or(usize::MAX),
                    diagnostic.line,
                    diagnostic.column,
                )
            });
            return Err(self
                .diagnostics
                .into_iter()
                .map(|(_, diagnostic)| diagnostic)
                .collect());
        }
        let mut server = items.server.map(|item| item.value).unwrap_or_default();
        server
            .node_id
            .get_or_insert_with(|| environment.hostname.clone());
        Ok(Config {
            server,
            components: values(items.components),
            groups: values(items.groups),
            checks: values(items.checks),
            heartbeats: values(items.heartbeats),
            projects: values(items.projects),
            notifiers: values(items.notifiers),
            routes: values(items.routes),
            tokens: values(items.tokens),
            page: items.page.map(|item| item.value).unwrap_or_default(),
        })
    }
}

fn values<T>(items: Vec<Item<T>>) -> Vec<T> {
    items.into_iter().map(|item| item.value).collect()
}

/// The steps from `value` down to the key or value whose span is `span`.
fn steps_to(value: &Spanned<DeValue<'_>>, span: &Range<usize>) -> Option<Vec<Step>> {
    if value.span() == *span {
        return Some(Vec::new());
    }
    match value.get_ref() {
        DeValue::Table(table) => table.iter().find_map(|(key, child)| {
            let step = Step::Key(key.get_ref().to_string());
            if key.span() == *span {
                return Some(vec![step]);
            }
            steps_to(child, span).map(|mut rest| {
                rest.insert(0, step);
                rest
            })
        }),
        DeValue::Array(array) => array.iter().enumerate().find_map(|(index, child)| {
            steps_to(child, span).map(|mut rest| {
                rest.insert(0, Step::Index(index));
                rest
            })
        }),
        _ => None,
    }
}

/// Every string under `value` containing `${ENV:`, by origin.
fn interpolations(value: &Spanned<DeValue<'_>>, origin: &Origin, found: &mut Vec<Origin>) {
    match value.get_ref() {
        DeValue::String(text) if text.contains("${ENV:") => found.push(origin.clone()),
        DeValue::Table(table) => {
            for (key, child) in table {
                interpolations(child, &origin.key(key.get_ref()), found);
            }
        }
        DeValue::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                interpolations(child, &origin.index(index), found);
            }
        }
        _ => {}
    }
}

/// A hint for a deserialization error: the nearest valid name for an
/// unknown field or variant.
fn hint_for(message: &str) -> Option<String> {
    if !(message.starts_with("unknown field") || message.starts_with("unknown variant")) {
        return None;
    }
    // serde writes "unknown field `x`, expected one of `a`, `b`".
    let mut names = message.split('`').skip(1).step_by(2);
    let given = names.next()?;
    let expected: Vec<&str> = names.collect();
    closest(given, expected.iter().copied()).map(|near| format!("did you mean `{near}`?"))
}

/// The candidate nearest `given` by edit distance, if any is near enough to
/// be a likely typo.
/// Why a `body` or `banner` assertion is invalid, if it is.
fn text_problem(text: &TextAssertion) -> Option<(String, Option<String>)> {
    if text.matcher_count() != 1 {
        return Some((
            "give exactly one of contains, not_contains or regex".to_owned(),
            None,
        ));
    }
    let pattern = text.regex.as_ref()?;
    crate::probe::regular_expression(pattern)
        .err()
        .map(|message| (message, None))
}

/// `path` with every dot step RFC 9535 rejects (a name holding `-` or
/// starting with a digit, such as `.x-request-id`) in bracket notation,
/// when it has one.
fn bracketed(path: &str) -> Option<String> {
    let mut fixed = String::with_capacity(path.len() + 8);
    let mut changed = false;
    let mut rest = path;
    while let Some(dot) = rest.find('.') {
        fixed.push_str(&rest[..dot]);
        let after = &rest[dot + 1..];
        let end = after
            .find(|character: char| {
                !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
            })
            .unwrap_or(after.len());
        let name = &after[..end];
        if name.contains('-') || name.starts_with(|character: char| character.is_ascii_digit()) {
            // A descendant step (`..name`) keeps its first dot.
            if fixed.ends_with('.') {
                fixed.push('.');
            }
            fixed.push_str("['");
            fixed.push_str(name);
            fixed.push_str("']");
            changed = true;
        } else {
            fixed.push('.');
            fixed.push_str(name);
        }
        rest = &after[end..];
    }
    fixed.push_str(rest);
    changed.then_some(fixed)
}

/// The candidate nearest to `given` by edit distance, if any is close
/// enough to be a likely typo.
pub fn closest<'c>(given: &str, candidates: impl Iterator<Item = &'c str>) -> Option<&'c str> {
    let limit = (given.chars().count() / 3).max(2);
    candidates
        .map(|candidate| (edit_distance(given, candidate), candidate))
        .filter(|(distance, _)| *distance <= limit)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// Levenshtein distance, in characters.
fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (row, left_character) in left.chars().enumerate() {
        let mut current = vec![row + 1];
        for (column, right_character) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_character != *right_character);
            current.push(
                substitution
                    .min(previous[column + 1] + 1)
                    .min(current[column] + 1),
            );
        }
        previous = current;
    }
    previous[right.len()]
}

fn list<T: fmt::Display>(names: impl IntoIterator<Item = T>) -> String {
    names
        .into_iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// An `http://` or `https://` URL with a host.
fn http_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    let host = rest.map(|rest| rest.split(['/', '?', '#']).next().unwrap_or_default());
    match host {
        Some(host) if !host.is_empty() && !url.contains(char::is_whitespace) => Ok(()),
        _ => Err(format!("`{url}` is not an http:// or https:// URL")),
    }
}

/// Whether `text` is a hostname or IP address.
fn is_host(text: &str) -> bool {
    text.parse::<std::net::IpAddr>().is_ok()
        || (!text.is_empty()
            && !text.starts_with(['-', '.'])
            && text.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '_')
            }))
}

/// Splits a target or resolver written `host`, `host:port`, `ipv6`,
/// `[ipv6]` or `[ipv6]:port` into its host and port; the port is `None`
/// when absent. Returns `None` when the port is not a number from 1 to
/// 65535.
#[must_use]
pub fn host_and_port(text: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = text.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after {
            "" => None,
            _ => Some(
                after
                    .strip_prefix(':')?
                    .parse()
                    .ok()
                    .filter(|port| *port != 0)?,
            ),
        };
        return Some((host, port));
    }
    if text.parse::<std::net::Ipv6Addr>().is_ok() {
        return Some((text, None));
    }
    match text.rsplit_once(':') {
        Some((host, port)) => Some((host, Some(port.parse().ok().filter(|port| *port != 0)?))),
        None => Some((text, None)),
    }
}

/// Whether `target` has the form this check type needs.
fn target(check_type: CheckType, target: &str) -> Result<(), String> {
    let valid = match check_type {
        CheckType::Http => return http_url(target),
        CheckType::Tcp => {
            host_and_port(target).is_some_and(|(host, port)| is_host(host) && port.is_some())
        }
        CheckType::Tls => host_and_port(target).is_some_and(|(host, _)| is_host(host)),
        CheckType::Dns => is_host(target) && target.parse::<std::net::IpAddr>().is_err(),
        CheckType::Icmp => is_host(target),
    };
    if valid {
        Ok(())
    } else {
        Err(format!(
            "`{target}` is not a valid {} target",
            check_type.as_str()
        ))
    }
}

fn target_example(check_type: CheckType) -> String {
    match check_type {
        CheckType::Http => "an http check targets a URL, such as \"https://example.com/health\"",
        CheckType::Tcp => "a tcp check targets host:port, such as \"db.example.com:5432\"",
        CheckType::Tls => "a tls check targets a host or host:port, such as \"example.com:443\"",
        CheckType::Dns => "a dns check targets a domain name, such as \"example.com\"",
        CheckType::Icmp => "an icmp check targets a host, such as \"192.0.2.1\" or \"example.com\"",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        Environment, Source, TOP_LEVEL_KEYS, closest, edit_distance, host_and_port, http_url, load,
        target,
    };
    use crate::config::{CheckType, schema};

    /// The kinds are listed by hand in `TOP_LEVEL_KEYS`, `Items`,
    /// `Loader::collect`, `Loader::into_result`, `ConfigFile` (the schema)
    /// and `Config`, as each kind is its own type. Leaving a kind out of
    /// any of them fails here.
    #[test]
    fn every_top_level_kind_is_published_collected_and_returned() {
        let samples = [
            ("server", "[server]\nnode_id = \"sample\"\n"),
            ("page", "[page]\ntitle = \"Sample\"\n"),
            ("group", "[[group]]\nid = \"sample\"\nname = \"Sample\"\n"),
            (
                "component",
                "[[component]]\nid = \"sample\"\nname = \"Sample\"\n",
            ),
            (
                "check",
                "[[check]]\nid = \"sample\"\ntype = \"icmp\"\ntarget = \"192.0.2.1\"\n",
            ),
            (
                "heartbeat",
                "[[heartbeat]]\nid = \"sample\"\ninterval = \"1m\"\n",
            ),
            (
                "project",
                "[[project]]\nid = \"sample\"\nname = \"Sample\"\n",
            ),
            (
                "notifier",
                "[[notifier]]\nid = \"sample\"\ntype = \"ntfy\"\n[notifier.ntfy]\nurl = \"https://ntfy.sh/sample\"\n",
            ),
            (
                "route",
                "[[route]]\nid = \"sample\"\nnotifiers = [\"sample\"]\n",
            ),
            (
                "token",
                "[[token]]\nid = \"sample\"\nsecret = \"${ENV:SAMPLE}\"\nscopes = [\"release\"]\n",
            ),
        ];
        let kinds: BTreeSet<&str> = TOP_LEVEL_KEYS.iter().copied().collect();
        let covered: BTreeSet<&str> = samples.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(covered, kinds, "every kind needs a sample here");

        let expected: BTreeSet<String> = kinds.iter().map(|kind| (*kind).to_owned()).collect();
        let published: BTreeSet<String> = schema()["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(published, expected, "ConfigFile");

        let variable = |_: &str| Some("sample-secret".to_owned());
        let environment = Environment {
            hostname: "host".to_owned(),
            variable: &variable,
        };
        let render = |texts: &[&str]| {
            let sources: Vec<Source> = texts
                .iter()
                .map(|text| Source {
                    name: "sample.toml".to_owned(),
                    text: Ok((*text).to_owned()),
                })
                .collect();
            serde_json::to_value(load(&sources, &environment).expect("sample is valid")).unwrap()
        };
        let empty = render(&[]);
        let returned: BTreeSet<String> = empty.as_object().unwrap().keys().cloned().collect();
        assert_eq!(returned, expected, "Config");

        let notifier = samples[7].1;
        for (kind, text) in samples {
            // A route needs the notifier it names.
            let texts: &[&str] = if kind == "route" {
                &[text, notifier]
            } else {
                &[text]
            };
            assert_ne!(render(texts)[kind], empty[kind], "{kind} was not collected");
        }
    }

    #[test]
    fn edit_distances() {
        assert_eq!(edit_distance("interval", "interval"), 0);
        assert_eq!(edit_distance("intervall", "interval"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(
            closest("compnent", ["check", "component"].into_iter()),
            Some("component")
        );
        assert_eq!(closest("zzzz", ["check", "component"].into_iter()), None);
    }

    #[test]
    fn targets() {
        assert!(http_url("https://example.com/health").is_ok());
        assert!(http_url("http://127.0.0.1:8080").is_ok());
        assert!(http_url("example.com").is_err());
        assert!(http_url("https://").is_err());
        assert_eq!(host_and_port("[::1]:443"), Some(("::1", Some(443))));
        assert_eq!(host_and_port("::1"), Some(("::1", None)));
        assert_eq!(host_and_port("db:0"), None);
        assert!(target(CheckType::Tcp, "db.example.com:5432").is_ok());
        assert!(target(CheckType::Tcp, "db.example.com").is_err());
        assert!(target(CheckType::Tls, "example.com").is_ok());
        assert!(target(CheckType::Tls, "example.com:443").is_ok());
        assert!(target(CheckType::Dns, "example.com").is_ok());
        assert!(target(CheckType::Dns, "192.0.2.1").is_err());
        assert!(target(CheckType::Icmp, "192.0.2.1").is_ok());
        assert!(target(CheckType::Icmp, "https://example.com").is_err());
    }
}
