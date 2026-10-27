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

use super::alerting::{
    Alerting, AlertingRule, CLASS_LABEL, OWN_ANNOTATIONS, OWN_LABELS, TOPOLOGY_NODE_LABEL,
    is_label_name,
};
use super::assertion::{Condition, TextAssertion};
use super::check::{CheckType, MAX_BODY_CEILING};
use super::colour::{AA_TEXT, DARK, LIGHT};
use super::diagnostic::{Diagnostic, LineIndex};
use super::notifier::NotifierType;
use super::telemetry::OWN_RESOURCE_ATTRIBUTES;
use super::values::{Id, Secret, Text, email_address, is_unanswerable};
use super::{
    Auth, AuthMode, Check, Component, Config, Group, Heartbeat, Link, MAX_APP_NAME, Node,
    NodeIdSource, Notifier, Page, Project, Route, Server, Slack, Subscribers, Telemetry, Token,
    Topology, TopologyNode,
};
use crate::link::{http_url, site_path};

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
    /// The `node_id` when nothing else gives one.
    pub hostname: String,
    /// The `node_id` given by `--node` or `TOCSIN_NODE_ID`, which wins over
    /// `[server] node_id`.
    pub node_id: Option<String>,
    /// Looks up an environment variable, for `${ENV:NAME}` secrets.
    pub variable: &'a dyn Fn(&str) -> Option<String>,
}

/// The top-level keys a file may hold, in the order they are processed.
pub(crate) const TOP_LEVEL_KEYS: &[&str] = &[
    "server",
    "node",
    "page",
    "group",
    "component",
    "check",
    "heartbeat",
    "project",
    "notifier",
    "route",
    "token",
    "subscribers",
    "slack",
    "telemetry",
    "alerting",
    "topology",
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
    loader.validate_node_id(&items, environment);
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
    nodes: Vec<Item<Node>>,
    page: Option<Item<Page>>,
    groups: Vec<Item<Group>>,
    components: Vec<Item<Component>>,
    checks: Vec<Item<Check>>,
    heartbeats: Vec<Item<Heartbeat>>,
    projects: Vec<Item<Project>>,
    notifiers: Vec<Item<Notifier>>,
    routes: Vec<Item<Route>>,
    tokens: Vec<Item<Token>>,
    subscribers: Option<Item<Subscribers>>,
    slack: Option<Item<Slack>>,
    telemetry: Option<Item<Telemetry>>,
    alerting: Option<Item<Alerting>>,
    topology: Option<Item<Topology>>,
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
                    "node" => items.nodes.extend(self.array(&origin, "node")),
                    "page" => items.page = self.singleton(&origin, "page", items.page.take()),
                    "group" => items.groups.extend(self.array(&origin, "group")),
                    "component" => items.components.extend(self.array(&origin, "component")),
                    "check" => items.checks.extend(self.array(&origin, "check")),
                    "heartbeat" => items.heartbeats.extend(self.array(&origin, "heartbeat")),
                    "project" => items.projects.extend(self.array(&origin, "project")),
                    "notifier" => items.notifiers.extend(self.array(&origin, "notifier")),
                    "route" => items.routes.extend(self.array(&origin, "route")),
                    "token" => items.tokens.extend(self.array(&origin, "token")),
                    "subscribers" => {
                        items.subscribers =
                            self.singleton(&origin, "subscribers", items.subscribers.take());
                    }
                    "slack" => items.slack = self.singleton(&origin, "slack", items.slack.take()),
                    "telemetry" => {
                        items.telemetry =
                            self.singleton(&origin, "telemetry", items.telemetry.take());
                    }
                    "alerting" => {
                        items.alerting = self.singleton(&origin, "alerting", items.alerting.take());
                    }
                    "topology" => {
                        items.topology = self.singleton(&origin, "topology", items.topology.take());
                    }
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
        // Checks and heartbeats share one history (`check_result.check_id`)
        // and one member list per component, so their ids must not collide.
        let shared = match kind {
            "check" => Some("heartbeat"),
            "heartbeat" => Some("check"),
            _ => None,
        };
        if let Some(other) = shared
            && let Some(first) = self.ids.get(other).and_then(|ids| ids.get(id)).cloned()
        {
            let message = format!(
                "{kind} id `{id}` is already a {other} id, defined at {}",
                self.describe(&first)
            );
            self.report(
                &origin,
                message,
                Some(
                    "checks and heartbeats share one id space: their results are stored together"
                        .to_owned(),
                ),
            );
            return;
        }
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
            for (index, component) in project.value.components.iter().enumerate() {
                references.push((
                    "component",
                    component,
                    project.origin.key("components").index(index),
                ));
            }
        }
        if let Some(page) = &items.page {
            let lists = [
                ("group", "groups", &page.value.groups),
                ("component", "components", &page.value.components),
            ];
            for (kind, key, ids) in lists {
                for (index, id) in ids.iter().enumerate() {
                    references.push((kind, id, page.origin.key(key).index(index)));
                }
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
        if let Some(subscribers) = &items.subscribers {
            self.validate_subscribers(subscribers, items.server.as_ref());
        }
        if let Some(slack) = &items.slack {
            self.validate_slack(slack, items);
        }
        if let Some(telemetry) = &items.telemetry {
            self.validate_telemetry(telemetry);
        }
        if let Some(page) = &items.page {
            self.validate_page(page);
        }
        for project in &items.projects {
            if let Some(url) = &project.value.url {
                self.url(&project.origin.key("url"), url);
            }
        }
        let alerting = items.alerting.as_ref().map(|alerting| &alerting.value);
        if let Some(alerting) = &items.alerting {
            self.validate_alerting(alerting);
        }
        self.validate_topology_nodes(items);
        for check in &mut items.checks {
            self.validate_check(check);
            self.validate_links(&check.origin, &check.value.links);
            let value = &check.value;
            self.validate_alert_fields(
                &check.origin,
                &format!("check `{}`", value.id),
                [&value.labels, &value.annotations],
                alerting,
            );
        }
        for heartbeat in &items.heartbeats {
            self.validate_links(&heartbeat.origin, &heartbeat.value.links);
            let value = &heartbeat.value;
            self.validate_alert_fields(
                &heartbeat.origin,
                &format!("heartbeat `{}`", value.id),
                [&value.labels, &value.annotations],
                alerting,
            );
            if heartbeat.value.interval.get() < std::time::Duration::from_secs(1) {
                self.report(
                    &heartbeat.origin.key("interval"),
                    "a heartbeat interval must be at least 1s",
                    None,
                );
            }
            self.validate_heartbeat_retention(heartbeat, items.server.as_ref());
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
            let repeat = route.value.repeat.get();
            if !repeat.is_zero() && repeat < std::time::Duration::from_mins(1) {
                self.report(
                    &route.origin.key("repeat"),
                    "`repeat` must be at least 1m, or \"0s\" to never repeat",
                    None,
                );
            }
        }
        for token in &items.tokens {
            if token.value.scopes.is_empty() {
                self.report(
                    &token.origin.key("scopes"),
                    "a token needs at least one scope",
                    Some("scopes are heartbeat, incident, incident:draft, release, silence, status:read and subscribers:admin".to_owned()),
                );
            }
        }
        // DNS names are case-insensitive but a token's `iss` is compared
        // exactly: Access issues it in lowercase, so the team is kept so.
        if let Some(team) = items
            .server
            .as_mut()
            .and_then(|server| server.value.auth.as_mut())
            .and_then(|auth| auth.team_domain.as_mut())
        {
            team.make_ascii_lowercase();
        }
    }

    /// A heartbeat's deadline runs from its latest stored result, so that
    /// result must outlive the wait: with `interval + grace` past half the
    /// retention, a restart can find the anchor swept, and a late miss can
    /// fall before the retention watermark the store refuses to write
    /// behind.
    fn validate_heartbeat_retention(
        &mut self,
        heartbeat: &Item<Heartbeat>,
        server: Option<&Item<Server>>,
    ) {
        let retention_days = server.map_or_else(
            || Server::default().retention_days,
            |server| server.value.retention_days,
        );
        if retention_days == 0 {
            return; // reported on its own
        }
        let value = &heartbeat.value;
        let allowance = value.interval.get().saturating_add(value.grace.get());
        let half = std::time::Duration::from_secs(u64::from(retention_days) * 43_200);
        if allowance <= half {
            return;
        }
        // Without the key, `describe` names the `[server]` table, where it
        // would go.
        let retention = match server {
            Some(server) => format!(
                "`[server] retention_days = {retention_days}` ({})",
                self.describe(&server.origin.key("retention_days"))
            ),
            None => format!("the default `[server] retention_days = {retention_days}`"),
        };
        self.report(
            &heartbeat.origin.key("interval"),
            format!(
                "heartbeat `{}`: `interval` ({}) + `grace` ({}) must be at most half of {retention}",
                value.id, value.interval, value.grace
            ),
            Some(
                "its deadline runs from its latest stored result, which retention must not sweep first: shorten `interval` or `grace`, or raise `retention_days`"
                    .to_owned(),
            ),
        );
    }

    /// The `node_id` this server runs as. A single server may be named
    /// anything without whitespace, and defaults to its hostname; once any
    /// `[[node]]` is listed, it must be named explicitly, as one of them.
    fn validate_node_id(&mut self, items: &Items, environment: &Environment<'_>) {
        let (node_id, source) = node_id(items, environment);
        // Where a problem with the id is reported: the `node_id` key, or
        // nowhere in a file when it came from the command line.
        let located = match source {
            NodeIdSource::Config => items
                .server
                .as_ref()
                .map(|server| server.origin.key("node_id")),
            NodeIdSource::Argument | NodeIdSource::Hostname => None,
        };
        let given_as = match source {
            NodeIdSource::Argument => "the node id given by `--node` or `TOCSIN_NODE_ID`",
            NodeIdSource::Config | NodeIdSource::Hostname => "`node_id`",
        };
        if node_id.is_empty() || node_id.contains(char::is_whitespace) {
            // `validate_server` has already reported a `node_id` written so.
            if source == NodeIdSource::Argument {
                self.report_node_id(
                    located.as_ref(),
                    format!("{given_as} must be non-empty with no whitespace"),
                    None,
                );
            }
            return;
        }
        let Some(first) = items.nodes.first() else {
            return;
        };
        let listed: Vec<&str> = items
            .nodes
            .iter()
            .map(|node| node.value.id.as_str())
            .collect();
        if source == NodeIdSource::Hostname {
            self.report(
                &first.origin,
                format!(
                    "`[[node]]` is listed, so this server's node id must be given; \
                     it would otherwise be the hostname `{node_id}`"
                ),
                Some(format!(
                    "pass `--node <id>`, set TOCSIN_NODE_ID or set `node_id` in `[server]` \
                     to one of {}",
                    list(&listed)
                )),
            );
            return;
        }
        if let Err(problem) = Id::parse(node_id) {
            self.report_node_id(
                located.as_ref(),
                format!("{given_as} `{node_id}` is not a node id: {problem}"),
                Some(format!("the listed nodes are {}", list(&listed))),
            );
            return;
        }
        if listed.contains(&node_id) {
            return;
        }
        let hint = closest(node_id, listed.iter().copied()).map_or_else(
            || {
                format!(
                    "it must be one of {}; add a `[[node]]` if it is new",
                    list(&listed)
                )
            },
            |near| format!("did you mean `{near}`?"),
        );
        self.report_node_id(
            located.as_ref(),
            format!("{given_as} `{node_id}` is not a listed `[[node]]`"),
            Some(hint),
        );
    }

    /// Reports a problem with the `node_id` at `origin`, or with no file
    /// position when it came from the command line.
    fn report_node_id(&mut self, origin: Option<&Origin>, message: String, hint: Option<String>) {
        if let Some(origin) = origin {
            self.report(origin, message, hint);
        } else {
            self.diagnostics.push((
                None,
                Diagnostic {
                    file: None,
                    line: None,
                    column: None,
                    path: Some("node_id".to_owned()),
                    message,
                    hint,
                },
            ));
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
        if value.max_silence.get().is_zero() {
            self.report(
                &origin.key("max_silence"),
                "`max_silence` must be longer than 0s",
                None,
            );
        }
        if let Some(auth) = &value.auth {
            self.validate_auth(&origin.key("auth"), auth);
            if auth.mode != AuthMode::None && value.public_url.is_none() {
                self.report(
                    &origin.keys(&["auth", "mode"]),
                    "the admin page needs `[server] public_url`: its forms are accepted only from that origin",
                    Some("add `public_url = \"https://status.example.com\"` under `[server]`, the address people open".to_owned()),
                );
            }
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

    /// `[server.auth]`: each mode has its required keys, and no key that
    /// belongs to another mode, so a half-edited table never silently means
    /// something else.
    fn validate_auth(&mut self, origin: &Origin, auth: &Auth) {
        let mode = match auth.mode {
            AuthMode::None => "none",
            AuthMode::CloudflareAccess => "cloudflare_access",
            AuthMode::TrustedHeader => "trusted_header",
        };
        let cloudflare = auth.mode == AuthMode::CloudflareAccess;
        let trusted = auth.mode == AuthMode::TrustedHeader;
        let keys = [
            ("team_domain", auth.team_domain.is_some(), cloudflare),
            ("audience", auth.audience.is_some(), cloudflare),
            ("header", auth.header.is_some(), trusted),
            ("trusted_proxies", !auth.trusted_proxies.is_empty(), trusted),
            (
                "allowed_emails",
                !auth.allowed_emails.is_empty(),
                auth.mode != AuthMode::None,
            ),
            (
                "allowed_domains",
                !auth.allowed_domains.is_empty(),
                auth.mode != AuthMode::None,
            ),
        ];
        for (key, present, belongs) in keys {
            if present && !belongs {
                self.report(
                    &origin.key(key),
                    format!("`{key}` does not apply when `mode = \"{mode}\"`"),
                    (auth.mode == AuthMode::None).then(|| {
                        "set `mode` to \"cloudflare_access\" or \"trusted_header\" to turn the admin page on"
                            .to_owned()
                    }),
                );
            }
        }
        if cloudflare {
            self.validate_access(origin, auth);
        }
        if trusted {
            self.validate_trusted_header(origin, auth);
        }
        for (index, address) in auth.allowed_emails.iter().enumerate() {
            match email_address(address) {
                Ok(bare) if bare == address => {}
                Ok(_) => self.report(
                    &origin.key("allowed_emails").index(index),
                    format!("`{address}` must be a bare address such as alice@example.com"),
                    None,
                ),
                Err(message) => {
                    self.report(&origin.key("allowed_emails").index(index), message, None);
                }
            }
        }
        for (index, domain) in auth.allowed_domains.iter().enumerate() {
            if !is_domain(domain) {
                self.report(
                    &origin.key("allowed_domains").index(index),
                    format!("`{domain}` is not a domain name such as example.com"),
                    domain
                        .strip_prefix('@')
                        .map(|bare| format!("drop the `@`: \"{bare}\"")),
                );
            }
        }
    }

    fn validate_access(&mut self, origin: &Origin, auth: &Auth) {
        match &auth.team_domain {
            None => self.report(
                origin,
                "`mode = \"cloudflare_access\"` needs `team_domain`",
                Some(
                    "add `team_domain = \"your-team\"`, the name in your-team.cloudflareaccess.com"
                        .to_owned(),
                ),
            ),
            Some(team) => {
                let lowercase = team.to_ascii_lowercase();
                let name = lowercase
                    .strip_suffix(".cloudflareaccess.com")
                    .unwrap_or(&lowercase);
                if !is_label(name) {
                    self.report(
                        &origin.key("team_domain"),
                        format!(
                            "`{team}` is not a Cloudflare Access team: write `name` or `name.cloudflareaccess.com`"
                        ),
                        None,
                    );
                }
            }
        }
        match &auth.audience {
            None => self.report(
                origin,
                "`mode = \"cloudflare_access\"` needs `audience`",
                Some(
                    "copy the Application Audience (AUD) tag from the Access application"
                        .to_owned(),
                ),
            ),
            Some(audience) if audience.is_empty() || audience.contains(char::is_whitespace) => self
                .report(
                    &origin.key("audience"),
                    "`audience` must be the AUD tag, non-empty with no whitespace",
                    None,
                ),
            Some(_) => {}
        }
    }

    fn validate_trusted_header(&mut self, origin: &Origin, auth: &Auth) {
        if auth.trusted_proxies.is_empty() {
            self.report(
                origin,
                "`mode = \"trusted_header\"` needs `trusted_proxies`, the networks your proxy connects from",
                Some("add `trusted_proxies = [\"127.0.0.1/32\"]` for a proxy on the same host".to_owned()),
            );
        }
        let Some(header) = &auth.header else { return };
        let origin = origin.key("header");
        if http::HeaderName::from_bytes(header.as_bytes()).is_err() {
            self.report(&origin, format!("`{header}` is not a header name"), None);
        } else if header.eq_ignore_ascii_case("cf-access-authenticated-user-email") {
            // Anything that reaches the origin without passing through
            // Access can send this header; only the signed token proves it.
            self.report(
                &origin,
                "`Cf-Access-Authenticated-User-Email` proves nothing on its own",
                Some(
                    "use `mode = \"cloudflare_access\"`, which verifies the signed token"
                        .to_owned(),
                ),
            );
        }
    }

    /// `[server.email]` is required, complete, once any notifier sends email.
    /// Every sender of email, an email notifier or `[subscribers]`, needs
    /// a complete `[server.email]`; the first one found is named.
    fn validate_email_senders(&mut self, items: &Items) {
        let sender = items
            .notifiers
            .iter()
            .find(|notifier| notifier.value.notifier_type == NotifierType::Email)
            .map(|first| {
                (
                    first.origin.key("type"),
                    format!("notifier `{}`", first.value.id),
                )
            })
            .or_else(|| {
                items
                    .subscribers
                    .as_ref()
                    .map(|subscribers| (subscribers.origin.clone(), "`[subscribers]`".to_owned()))
            });
        let Some((origin, sender)) = sender else {
            return;
        };
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
                &origin,
                format!("{sender} sends email, but no file sets `[server.email]`"),
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
                    format!("`[server.email]` needs `{key}` because {sender} sends email"),
                    None,
                );
            }
        }
    }

    /// `[slack]`: Slack sends people back to an `https` public URL, and
    /// someone must be able to connect a channel: admins, behind
    /// `[server.auth]`, or visitors, with `[subscribers]`.
    fn validate_slack(&mut self, slack: &Item<Slack>, items: &Items) {
        let origin = &slack.origin;
        let server = items.server.as_ref();
        let server_unknown =
            server.is_none() && (self.singletons.contains_key("server") || self.incomplete);
        let public_url = server.and_then(|server| server.value.public_url.as_ref());
        match public_url {
            None if !server_unknown => self.report(
                origin,
                "`[slack]` needs `[server] public_url`: Slack sends people back to it after they approve",
                Some("add `public_url = \"https://status.example.com\"` under `[server]`, the address people open".to_owned()),
            ),
            Some(url) if !url.starts_with("https://") => {
                let server = server.expect("public_url came from [server]");
                self.report(
                    &server.origin.key("public_url"),
                    "`[slack]` needs an `https` `public_url`: Slack sends people back only to https addresses",
                    None,
                );
            }
            _ => {}
        }
        let admin = server
            .and_then(|server| server.value.auth.as_ref())
            .is_some_and(|auth| auth.mode != AuthMode::None);
        let subscribers =
            items.subscribers.is_some() || self.singletons.contains_key("subscribers");
        if !admin && !subscribers && !server_unknown && !self.incomplete {
            self.report(
                origin,
                "`[slack]` has no one to connect a channel: it needs the admin page or `[subscribers]`",
                Some("set `[server.auth]` so admins can connect channels for notifiers, or add `[subscribers]` so visitors can add Slack".to_owned()),
            );
        }
        if let Some(name) = &slack.value.app_name {
            let length = name.chars().count();
            if name.trim().is_empty() || length > MAX_APP_NAME {
                self.report(
                    &origin.key("app_name"),
                    format!("`app_name` must be 1 to {MAX_APP_NAME} characters, as Slack requires; this one is {length}"),
                    None,
                );
            }
        }
    }

    /// `[telemetry.otlp]`: what the file sets must make a working
    /// exporter on its own. What it leaves to the environment is checked
    /// when the exporter starts.
    fn validate_telemetry(&mut self, telemetry: &Item<Telemetry>) {
        let Some(otlp) = &telemetry.value.otlp else {
            return;
        };
        let origin = telemetry.origin.key("otlp");
        if let Some(endpoint) = &otlp.endpoint {
            self.url(&origin.key("endpoint"), endpoint);
        }
        if let Some(interval) = otlp.interval
            && interval.get() < std::time::Duration::from_secs(1)
        {
            self.report(
                &origin.key("interval"),
                "`interval` must be at least 1s",
                None,
            );
        }
        if let Some(timeout) = otlp.timeout {
            if timeout.get().is_zero() {
                self.report(&origin.key("timeout"), "`timeout` must be above 0", None);
            } else if let Some(interval) = otlp.interval
                && timeout.get() > interval.get()
            {
                self.report(
                    &origin.key("timeout"),
                    format!(
                        "`timeout` ({timeout}) must not be longer than `interval` ({interval})"
                    ),
                    Some(
                        "an export still running when the next one is due is abandoned".to_owned(),
                    ),
                );
            }
        }
        for name in otlp.resource.keys() {
            if name.trim().is_empty() {
                self.report(
                    &origin.key("resource"),
                    "a resource attribute needs a non-empty name",
                    None,
                );
            } else if OWN_RESOURCE_ATTRIBUTES.contains(&name.as_str()) {
                self.report(
                    &origin.keys(&["resource", name]),
                    format!("`{name}` is set by tocsin and cannot be overridden"),
                    Some(
                        "tocsin sets service.version, and service.instance.id to the node_id"
                            .to_owned(),
                    ),
                );
            }
        }
    }

    /// `[alerting]`: each name it covers must be one a check could set, and
    /// each allowed value one it could hold.
    fn validate_alerting(&mut self, alerting: &Item<Alerting>) {
        let value = &alerting.value;
        for (field, rules) in [
            (AlertField::Labels, &value.labels),
            (AlertField::Annotations, &value.annotations),
        ] {
            for (name, rule) in rules {
                let origin = alerting.origin.keys(&[field.key(), name]);
                if let Some((message, hint)) = field.name_problem(name) {
                    self.report(&origin, message, Some(hint));
                }
                if rule.values.iter().any(String::is_empty) {
                    self.report(
                        &origin.key("values"),
                        format!("{} `{name}` cannot allow an empty value", field.noun()),
                        Some(format!(
                            "Alertmanager drops an empty {}, so it would never arrive",
                            field.noun()
                        )),
                    );
                }
            }
        }
    }

    /// A check's or heartbeat's `labels` and `annotations`: names a receiver
    /// takes and tocsin does not set itself, and, with `[alerting]`, every
    /// one it requires, with a value it allows.
    fn validate_alert_fields(
        &mut self,
        origin: &Origin,
        subject: &str,
        fields: [&BTreeMap<String, String>; 2],
        alerting: Option<&Alerting>,
    ) {
        for (field, set) in [AlertField::Labels, AlertField::Annotations]
            .into_iter()
            .zip(fields)
        {
            let noun = field.noun();
            for (name, value) in set {
                let origin = origin.keys(&[field.key(), name]);
                if let Some((message, hint)) = field.name_problem(name) {
                    self.report(&origin, format!("{subject}: {message}"), Some(hint));
                }
                if value.is_empty() {
                    // The one diagnostic for it: the contract's value check
                    // skips an empty value, and its allowed values join here.
                    let rule = alerting.and_then(|alerting| field.rules(alerting).get(name));
                    self.report(
                        &origin,
                        format!("{subject}: {noun} `{name}` is empty"),
                        Some(format!(
                            "Alertmanager drops an empty {noun}: give it a value or remove it{}",
                            rule.map(allowed).unwrap_or_default()
                        )),
                    );
                }
            }
            let Some(alerting) = alerting else {
                continue;
            };
            for (name, rule) in field.rules(alerting) {
                let origin = origin.keys(&[field.key(), name]);
                match set.get(name) {
                    None if rule.required => {
                        // A misspelling is the likely cause: name it.
                        let hint = closest(
                            name,
                            set.keys()
                                .map(String::as_str)
                                .filter(|given| !field.rules(alerting).contains_key(*given)),
                        )
                        .map_or_else(
                            || {
                                format!(
                                    "add `{name} = \"...\"` to its `{}`{}",
                                    field.key(),
                                    allowed(rule)
                                )
                            },
                            |near| format!("`{near}` is set: did you mean `{name}`?"),
                        );
                        self.report(
                            &origin,
                            format!(
                                "{subject} is missing {noun} `{name}`, which `[alerting]` requires"
                            ),
                            Some(hint),
                        );
                    }
                    Some(value)
                        if !value.is_empty()
                            && !rule.values.is_empty()
                            && !rule.values.contains(value) =>
                    {
                        let hint = closest(value, rule.values.iter().map(String::as_str))
                            .map_or_else(
                                || format!("it must be one of {}", list(&rule.values)),
                                |near| {
                                    format!(
                                        "did you mean `{near}`? It must be one of {}",
                                        list(&rule.values)
                                    )
                                },
                            );
                        self.report(
                            &origin,
                            format!(
                                "{subject}: {noun} `{name}` is `{value}`, which `[alerting]` does not allow"
                            ),
                            Some(hint),
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    /// Every check's and heartbeat's `topology_node`: with `[topology]`, one
    /// of the nodes it lists.
    fn validate_topology_nodes(&mut self, items: &Items) {
        let Some(topology) = &items.topology else {
            return;
        };
        let checks = items.checks.iter().map(|check| {
            let value = &check.value;
            (
                &check.origin,
                format!("check `{}`", value.id),
                value.topology_node.as_ref(),
            )
        });
        let heartbeats = items.heartbeats.iter().map(|heartbeat| {
            let value = &heartbeat.value;
            (
                &heartbeat.origin,
                format!("heartbeat `{}`", value.id),
                value.topology_node.as_ref(),
            )
        });
        for (origin, subject, node) in checks.chain(heartbeats) {
            if let Some(node) = node {
                self.validate_topology_node(origin, &subject, node, &topology.value);
            }
        }
    }

    /// One `topology_node` against the nodes `[topology]` lists.
    fn validate_topology_node(
        &mut self,
        origin: &Origin,
        subject: &str,
        node: &TopologyNode,
        topology: &Topology,
    ) {
        if topology.nodes.contains(node) {
            return;
        }
        let hint = closest(
            node.as_str(),
            topology.nodes.iter().map(TopologyNode::as_str),
        )
        .map_or_else(
            || {
                format!(
                    "it must be one of the {} nodes `[topology]` lists; add it there if it is new",
                    topology.nodes.len()
                )
            },
            |near| format!("did you mean `{near}`?"),
        );
        self.report(
            &origin.key("topology_node"),
            format!("{subject}: topology node `{node}` is not in `[topology]`"),
            Some(hint),
        );
    }

    /// `[subscribers]`: its emails link back to the page, and its limits
    /// must let at least one sign-up through.
    fn validate_subscribers(
        &mut self,
        subscribers: &Item<Subscribers>,
        server: Option<&Item<Server>>,
    ) {
        let origin = &subscribers.origin;
        let value = &subscribers.value;
        let public_url = server.and_then(|server| server.value.public_url.as_ref());
        let server_unknown =
            server.is_none() && (self.singletons.contains_key("server") || self.incomplete);
        if public_url.is_none() && !server_unknown {
            self.report(
                origin,
                "`[subscribers]` needs `[server] public_url`: every email links back to the page",
                Some("add `public_url = \"https://status.example.com\"` under `[server]`, the address people open".to_owned()),
            );
        }
        for (key, limit) in [
            ("signups_per_ip", value.signups_per_ip),
            ("signups_per_email", value.signups_per_email),
        ] {
            if limit == 0 {
                self.report(
                    &origin.key(key),
                    format!("`{key}` must be at least 1"),
                    None,
                );
            }
        }
        if value.rate_window.get() < std::time::Duration::from_mins(1) {
            self.report(
                &origin.key("rate_window"),
                "`rate_window` must be at least 1m",
                None,
            );
        }
        let confirm = value.confirm_within.get();
        if confirm < std::time::Duration::from_mins(10)
            || confirm > std::time::Duration::from_hours(30 * 24)
        {
            self.report(
                &origin.key("confirm_within"),
                "`confirm_within` must be between 10m and 30d",
                None,
            );
        }
        match &value.client_ip_header {
            Some(header) => {
                if header.is_empty()
                    || !header
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
                {
                    self.report(
                        &origin.key("client_ip_header"),
                        format!("`client_ip_header` must be an HTTP header name, not `{header}`"),
                        None,
                    );
                }
                if value.trusted_proxies.is_empty() {
                    self.report(
                        &origin.key("client_ip_header"),
                        "`client_ip_header` needs `trusted_proxies`: without them anyone could set it",
                        Some("list the networks your proxy connects from, such as `trusted_proxies = [\"10.0.0.0/8\"]`".to_owned()),
                    );
                }
            }
            None => {
                if !value.trusted_proxies.is_empty() {
                    self.report(
                        &origin.key("trusted_proxies"),
                        "`trusted_proxies` does nothing without `client_ip_header`",
                        None,
                    );
                }
            }
        }
        if let Some(turnstile) = &value.turnstile
            && turnstile.site_key.trim().is_empty()
        {
            self.report(
                &origin.keys(&["turnstile", "site_key"]),
                "`site_key` must not be empty",
                None,
            );
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
        if let Some(http) = &value.http {
            let max_body = http.max_body;
            if max_body.bytes() == 0 {
                self.report(
                    &origin.keys(&["http", "max_body"]),
                    "`max_body` must be more than 0B",
                    None,
                );
            } else if max_body > MAX_BODY_CEILING {
                self.report(
                    &origin.keys(&["http", "max_body"]),
                    format!("`max_body` ({max_body}) must be at most {MAX_BODY_CEILING}"),
                    None,
                );
            }
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
        if value.max_in_flight == 0 {
            self.report(
                &origin.key("max_in_flight"),
                "`max_in_flight` must be at least 1",
                None,
            );
        }
        if let Some(pushover) = &value.pushover {
            let priorities = [
                ("down_priority", pushover.down_priority),
                ("degraded_priority", pushover.degraded_priority),
                ("resolved_priority", pushover.resolved_priority),
            ];
            for (key, priority) in priorities {
                if !(-2..=2).contains(&priority) {
                    self.report(
                        &origin.keys(&["pushover", key]),
                        "a Pushover priority is -2, -1, 0, 1 or 2",
                        None,
                    );
                }
            }
        }
        if let Some(slack) = &value.slack
            && slack.webhook_url.is_some() == slack.connection.is_some()
        {
            self.report(
                &origin.key("slack"),
                "`[notifier.slack]` needs `webhook_url` or `connection`, not both",
                Some("`connection` names a channel connected on the admin page; `webhook_url` is one you made yourself".to_owned()),
            );
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

    /// `[page]`: its URLs, a logo the page's content security policy will
    /// load, order lists without repeats, and a time zone the bundled
    /// database knows. The order lists' ids are checked with the other
    /// references.
    fn validate_page(&mut self, page: &Item<Page>) {
        let (origin, value) = (&page.origin, &page.value);
        if let Some(url) = &value.homepage_url {
            self.url(&origin.key("homepage_url"), url);
        }
        if let Some(logo) = &value.logo
            && let Err(message) = site_path(&logo.url)
        {
            self.report(
                &origin.keys(&["logo", "url"]),
                message,
                Some(
                    "the page loads images from its own origin only; serve the logo from the proxy in front of tocsin"
                        .to_owned(),
                ),
            );
        }
        self.validate_links(origin, &value.links);
        for (key, ids) in [("groups", &value.groups), ("components", &value.components)] {
            let mut seen = HashSet::new();
            for (index, id) in ids.iter().enumerate() {
                if !seen.insert(id.as_str()) {
                    self.report(
                        &origin.key(key).index(index),
                        format!("`{id}` is listed twice"),
                        None,
                    );
                }
            }
        }
        if jiff::tz::TimeZone::get(&value.timezone).is_err() {
            self.report(
                &origin.key("timezone"),
                format!("unknown time zone `{}`", value.timezone),
                Some("use an IANA name such as \"Europe/Berlin\", or \"UTC\"".to_owned()),
            );
        }
        for (key, colour, scheme, hint) in [
            ("accent", value.accent, LIGHT, "choose a darker colour"),
            (
                "accent_dark",
                value.accent_dark,
                DARK,
                "choose a lighter colour, or leave `accent_dark` out to derive it from `accent`",
            ),
        ] {
            let Some(colour) = colour else { continue };
            let (ratio, against, background) = scheme.weakest(colour);
            if ratio < AA_TEXT {
                // Floored, so a near miss never reads as a pass.
                let hundredths = (ratio * 100.0).floor() / 100.0;
                self.report(
                    &origin.key(key),
                    format!(
                        "`{colour}` has {hundredths:.2}:1 contrast with the {} scheme's {against} \
                         {background}; text needs at least {AA_TEXT}:1 (WCAG AA)",
                        scheme.name
                    ),
                    Some(hint.to_owned()),
                );
            }
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

    /// Rejects an `http` check's or `[telemetry.otlp]`'s header that could
    /// not be sent: a name that is not an HTTP token, or a value (secrets
    /// included, once resolved) holding a control character such as a
    /// newline. A secret's value is never repeated; its variable is named
    /// instead.
    fn validate_headers(&mut self, items: &Items) {
        if let Some(telemetry) = &items.telemetry
            && let Some(otlp) = &telemetry.value.otlp
        {
            for (name, value) in &otlp.headers {
                self.header(
                    &telemetry.origin.keys(&["otlp", "headers", name]),
                    name,
                    value,
                );
            }
        }
        for check in &items.checks {
            let Some(http) = &check.value.http else {
                continue;
            };
            for (name, value) in &http.headers {
                self.header(&check.origin.keys(&["http", "headers", name]), name, value);
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

    /// Reports a header that could not be sent: a name that is not an HTTP
    /// token, or a value holding a control character.
    fn header(&mut self, origin: &Origin, name: &str, value: &Text) {
        if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
            self.report(
                origin,
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
            self.header_value(origin, text, source);
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
        if let Some(subscribers) = &mut items.subscribers
            && let Some(turnstile) = &mut subscribers.value.turnstile
        {
            secrets.push((
                subscribers.origin.keys(&["turnstile", "secret_key"]),
                &mut turnstile.secret_key,
            ));
        }
        if let Some(slack) = &mut items.slack {
            secrets.push((slack.origin.key("client_id"), &mut slack.value.client_id));
            secrets.push((
                slack.origin.key("client_secret"),
                &mut slack.value.client_secret,
            ));
        }
        telemetry_secrets(&mut items.telemetry, &mut secrets);
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
        let (node_id, node_id_source) = node_id(&items, environment);
        let node_id = node_id.to_owned();
        let mut server = items.server.map(|item| item.value).unwrap_or_default();
        server.node_id = Some(node_id);
        Ok(Config {
            server,
            node_id_source,
            nodes: values(items.nodes),
            components: values(items.components),
            groups: values(items.groups),
            checks: values(items.checks),
            heartbeats: values(items.heartbeats),
            projects: values(items.projects),
            notifiers: values(items.notifiers),
            routes: values(items.routes),
            tokens: values(items.tokens),
            page: items.page.map(|item| item.value).unwrap_or_default(),
            subscribers: items.subscribers.map(|item| item.value),
            slack: items.slack.map(|item| item.value),
            telemetry: items.telemetry.map(|item| item.value),
            alerting: items.alerting.map(|item| item.value),
            topology: items.topology.map(|item| item.value),
        })
    }
}

/// The two maps a check or heartbeat carries onto its alerts.
#[derive(Debug, Clone, Copy)]
enum AlertField {
    Labels,
    Annotations,
}

impl AlertField {
    fn key(self) -> &'static str {
        match self {
            AlertField::Labels => "labels",
            AlertField::Annotations => "annotations",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            AlertField::Labels => "label",
            AlertField::Annotations => "annotation",
        }
    }

    /// What tocsin sets itself, which configuration may not.
    fn reserved(self) -> &'static [&'static str] {
        match self {
            AlertField::Labels => OWN_LABELS,
            AlertField::Annotations => OWN_ANNOTATIONS,
        }
    }

    fn rules(self, alerting: &Alerting) -> &BTreeMap<String, AlertingRule> {
        match self {
            AlertField::Labels => &alerting.labels,
            AlertField::Annotations => &alerting.annotations,
        }
    }

    /// Why `name` cannot be one, with a hint, if it cannot.
    fn name_problem(self, name: &str) -> Option<(String, String)> {
        let noun = self.noun();
        if !is_label_name(name) {
            return Some((
                format!("`{name}` is not a valid {noun} name"),
                "use letters, digits and underscores, not starting with a digit or `__`, as Alertmanager requires".to_owned(),
            ));
        }
        self.reserved().contains(&name).then(|| {
            let hint = if matches!(self, AlertField::Labels) && name == CLASS_LABEL {
                "set the check's or heartbeat's `class` instead".to_owned()
            } else if matches!(self, AlertField::Labels) && name == TOPOLOGY_NODE_LABEL {
                "set the check's or heartbeat's `topology_node` instead".to_owned()
            } else {
                format!(
                    "tocsin sets {} on every alert; choose another name",
                    list(self.reserved())
                )
            };
            (format!("{noun} `{name}` is set by tocsin"), hint)
        })
    }
}

/// `; it must be one of ...` when a rule limits the values, else nothing.
fn allowed(rule: &AlertingRule) -> String {
    if rule.values.is_empty() {
        String::new()
    } else {
        format!("; it must be one of {}", list(&rule.values))
    }
}

fn values<T>(items: Vec<Item<T>>) -> Vec<T> {
    items.into_iter().map(|item| item.value).collect()
}

/// The `node_id` this server runs as, and where it came from: `--node` or
/// `TOCSIN_NODE_ID`, then `[server] node_id`, then the hostname.
fn node_id<'a>(items: &'a Items, environment: &'a Environment<'_>) -> (&'a str, NodeIdSource) {
    if let Some(node_id) = &environment.node_id {
        return (node_id, NodeIdSource::Argument);
    }
    match items
        .server
        .as_ref()
        .and_then(|server| server.value.node_id.as_deref())
    {
        Some(node_id) => (node_id, NodeIdSource::Config),
        None => (&environment.hostname, NodeIdSource::Hostname),
    }
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
/// unknown field or variant, or how to make a required secret.
fn hint_for(message: &str) -> Option<String> {
    if message.starts_with("missing field `signing_secret`") {
        return Some(
            "generate one with `openssl rand -hex 32`, put it in an environment \
             variable and write `signing_secret = \"${ENV:NAME}\"`"
                .to_owned(),
        );
    }
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

/// One DNS label: letters, digits and inner hyphens.
fn is_label(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 63
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !text.starts_with('-')
        && !text.ends_with('-')
}

/// A domain name of at least two labels, such as `example.com`.
fn is_domain(text: &str) -> bool {
    text.len() <= 253 && text.contains('.') && text.split('.').all(is_label)
}

/// Adds the `${ENV:...}` header values of `[telemetry.otlp]`, if there
/// is one, to `secrets`.
fn telemetry_secrets<'a>(
    telemetry: &'a mut Option<Item<Telemetry>>,
    secrets: &mut Vec<(Origin, &'a mut Secret)>,
) {
    let Some(Item { origin, value }) = telemetry else {
        return;
    };
    let Some(otlp) = &mut value.otlp else {
        return;
    };
    for (name, value) in &mut otlp.headers {
        if let Text::Secret(secret) = value {
            secrets.push((origin.keys(&["otlp", "headers", name]), secret));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        Environment, Source, TOP_LEVEL_KEYS, closest, edit_distance, host_and_port, load, target,
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
            ("subscribers", "[subscribers]\n"),
            (
                "slack",
                "[slack]\nclient_id = \"${ENV:ID}\"\nclient_secret = \"${ENV:SECRET}\"\n",
            ),
            ("telemetry", "[telemetry.otlp]\n"),
            ("alerting", "[alerting.labels.team]\n"),
            ("topology", "[topology]\n"),
            (
                "node",
                "[server]\nnode_id = \"sample\"\n[[node]]\nid = \"sample\"\n",
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

        let variable = |name: &str| {
            Some(if name == "SMTP_URL" {
                "smtp://mail.example.com".to_owned()
            } else {
                "sample-secret".to_owned()
            })
        };
        let environment = Environment {
            hostname: "host".to_owned(),
            node_id: None,
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
        let sender = "[server]\npublic_url = \"https://status.example.com\"\n[server.email]\nfrom = \"status@example.com\"\nreply_to = \"support@example.com\"\nsmtp_url = \"${ENV:SMTP_URL}\"\n";
        for (kind, text) in samples {
            // A route needs the notifier it names; subscribers need a
            // sender and a page to link to.
            let texts: &[&str] = match kind {
                "route" => &[text, notifier],
                "subscribers" => &[text, sender],
                "slack" => &[text, sender, samples[10].1],
                _ => &[text],
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
