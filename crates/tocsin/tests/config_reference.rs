// SPDX-License-Identifier: AGPL-3.0-only

//! Generates docs/config-reference.md from schema/tocsin.schema.json and
//! fails when the committed page differs. Regenerate it with
//! `TOCSIN_UPDATE_DOCS=1 cargo test -p tocsin --test config_reference`.
//!
//! The schema is the source: every table, key, type, default and
//! description comes from it, in the schema's own (sorted) order, so the
//! page changes only when the schema does. A schema construct the page does
//! not know how to show fails the test rather than being left out.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

const PAGE: &str = "docs/config-reference.md";
const REGENERATE: &str = "TOCSIN_UPDATE_DOCS=1 cargo test -p tocsin --test config_reference";

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn the_committed_config_reference_is_current() {
    let schema = std::fs::read_to_string(repository().join("schema/tocsin.schema.json"))
        .expect("schema/tocsin.schema.json exists");
    let schema: Value = serde_json::from_str(&schema).expect("the schema is JSON");
    let generated = render(&schema);
    let path = repository().join(PAGE);
    if std::env::var_os("TOCSIN_UPDATE_DOCS").is_some() {
        std::fs::write(&path, &generated).expect("writes the page");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "{PAGE} is stale; regenerate it from the schema with\n  {REGENERATE}"
    );
}

#[test]
fn every_definition_appears_on_the_page() {
    let schema = std::fs::read_to_string(repository().join("schema/tocsin.schema.json"))
        .expect("schema/tocsin.schema.json exists");
    let schema: Value = serde_json::from_str(&schema).expect("the schema is JSON");
    let page = render(&schema);
    for name in definitions(&schema).keys() {
        let anchor = format!("<a id=\"{}\"></a>", anchor(&schema, name));
        assert!(page.contains(&anchor), "{name} has no section on the page");
    }
}

fn definitions(schema: &Value) -> &Map<String, Value> {
    schema["definitions"]
        .as_object()
        .expect("the schema has definitions")
}

/// The page, from the root of the schema.
fn render(schema: &Value) -> String {
    let mut tables = Tables::new(schema);
    tables.collect(
        &TablePath::root(),
        schema["properties"]
            .as_object()
            .expect("the root has properties"),
    );

    let mut page = String::new();
    writeln!(
        page,
        "<!-- Generated from schema/tocsin.schema.json; do not edit. Regenerate with\n     {REGENERATE} -->\n"
    )
    .unwrap();
    writeln!(page, "# Configuration reference\n").unwrap();
    writeln!(
        page,
        "Every key tocsin reads from its configuration folder, generated from \
         [the JSON Schema](../schema/tocsin.schema.json) (`tocsin config schema`). \
         [config.md](config.md) explains how the files fit together, with examples; \
         this page is the list.\n"
    )
    .unwrap();
    // The root's first paragraph; the rest addresses the schema's readers.
    let root = description(schema).split("\n\n").next().unwrap_or_default();
    writeln!(page, "{}\n", paragraph(root)).unwrap();
    writeln!(
        page,
        "A **required** key must be set in its table. A key with no default and not \
         required may be left out. `[[name]]` is an array of tables, written once per \
         item; `[name]` is a single table.\n"
    )
    .unwrap();

    writeln!(page, "## Tables\n").unwrap();
    for table in &tables.order {
        tables.render(&mut page, table);
    }

    writeln!(page, "## Value types\n").unwrap();
    writeln!(
        page,
        "Types a key's **Type** column links to: strings with a fixed form, and the \
         values a choice accepts.\n"
    )
    .unwrap();
    for (name, definition) in definitions(schema) {
        if !is_table(definition) {
            render_value_type(&mut page, schema, name, definition);
        }
    }
    page
}

/// Where a table sits in a file: `server.auth`, `check.assert.header`.
#[derive(Clone, Debug)]
struct TablePath {
    keys: Vec<String>,
    /// Whether the last key is an array of tables.
    array: bool,
}

impl TablePath {
    fn root() -> Self {
        Self {
            keys: Vec::new(),
            array: false,
        }
    }

    fn child(&self, key: &str, array: bool) -> Self {
        let mut keys = self.keys.clone();
        keys.push(key.to_owned());
        Self { keys, array }
    }

    fn header(&self) -> String {
        let dotted = self.keys.join(".");
        if self.array {
            format!("`[[{dotted}]]`")
        } else {
            format!("`[{dotted}]`")
        }
    }
}

/// A table section: one per object definition, placed where the schema
/// first reaches it, and naming every other place it appears.
struct Table<'a> {
    definition: &'a str,
    paths: Vec<TablePath>,
}

struct Tables<'a> {
    schema: &'a Value,
    by_definition: BTreeMap<&'a str, usize>,
    order: Vec<Table<'a>>,
}

impl<'a> Tables<'a> {
    fn new(schema: &'a Value) -> Self {
        Self {
            schema,
            by_definition: BTreeMap::new(),
            order: Vec::new(),
        }
    }

    /// Records, depth first, every table reachable from `properties`.
    fn collect(&mut self, parent: &TablePath, properties: &'a Map<String, Value>) {
        for (key, property) in properties {
            let Some((name, array)) = table_of(self.schema, property) else {
                continue;
            };
            let path = parent.child(key, array);
            if let Some(&index) = self.by_definition.get(name) {
                self.order[index].paths.push(path);
                continue;
            }
            self.by_definition.insert(name, self.order.len());
            self.order.push(Table {
                definition: name,
                paths: vec![path.clone()],
            });
            let definition = &definitions(self.schema)[name];
            self.collect(
                &path,
                definition["properties"]
                    .as_object()
                    .expect("a table has properties"),
            );
        }
    }

    fn render(&self, page: &mut String, table: &Table<'_>) {
        let definition = &definitions(self.schema)[table.definition];
        let first = &table.paths[0];
        let level = "#".repeat((first.keys.len() + 2).min(6));
        writeln!(
            page,
            "<a id=\"{}\"></a>\n\n{level} {}\n",
            anchor(self.schema, table.definition),
            first.header()
        )
        .unwrap();
        if table.paths.len() > 1 {
            let others: Vec<String> = table.paths[1..].iter().map(TablePath::header).collect();
            writeln!(page, "Also {}.\n", others.join(", ")).unwrap();
        }
        writeln!(page, "{}\n", paragraph(description(definition))).unwrap();
        let required: Vec<&str> = definition
            .get("required")
            .and_then(Value::as_array)
            .map(|keys| keys.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        writeln!(page, "| Key | Type | Required | Default | Description |").unwrap();
        writeln!(page, "|-----|------|----------|---------|-------------|").unwrap();
        for (key, property) in definition["properties"]
            .as_object()
            .expect("a table has properties")
        {
            writeln!(
                page,
                "| `{key}` | {} | {} | {} | {} |",
                type_of(self.schema, property),
                if required.contains(&key.as_str()) {
                    "yes"
                } else {
                    ""
                },
                property.get("default").map(default).unwrap_or_default(),
                cell(description(property)),
            )
            .unwrap();
        }
        writeln!(page).unwrap();
    }
}

/// The id of a definition's section.
fn anchor(schema: &Value, name: &str) -> String {
    let prefix = if is_table(&definitions(schema)[name]) {
        "table"
    } else {
        "type"
    };
    format!("{prefix}-{}", name.to_lowercase())
}

fn link(schema: &Value, name: &str, text: &str) -> String {
    format!("[{text}](#{})", anchor(schema, name))
}

fn is_table(definition: &Value) -> bool {
    definition.get("properties").is_some()
}

/// The definition a `$ref` names.
fn referenced(value: &Value) -> Option<&str> {
    value.get("$ref")?.as_str().map(|reference| {
        reference
            .strip_prefix("#/definitions/")
            .unwrap_or_else(|| panic!("unexpected reference {reference}"))
    })
}

/// A property's definition reference, through the wrappers schemars uses
/// for optional (`anyOf` with null) and defaulted (`allOf`) values.
fn unwrap_reference(property: &Value) -> Option<&str> {
    if let Some(name) = referenced(property) {
        return Some(name);
    }
    if let Some(all) = property.get("allOf").and_then(Value::as_array) {
        assert_eq!(all.len(), 1, "allOf with several schemas: {property}");
        return Some(
            referenced(&all[0]).unwrap_or_else(|| panic!("allOf without $ref: {property}")),
        );
    }
    if let Some(any) = property.get("anyOf").and_then(Value::as_array) {
        let mut names = any
            .iter()
            .filter(|schema| schema.get("type") != Some(&Value::from("null")))
            .map(|schema| {
                referenced(schema).unwrap_or_else(|| panic!("anyOf without $ref: {property}"))
            });
        let name = names.next();
        assert!(
            names.next().is_none(),
            "anyOf with several schemas: {property}"
        );
        return name;
    }
    None
}

/// The table a property holds, if it holds one, and whether it is an array
/// of tables.
fn table_of<'a>(schema: &'a Value, property: &'a Value) -> Option<(&'a str, bool)> {
    let (inner, array) = match property.get("items") {
        Some(items) => (items, true),
        None => (property, false),
    };
    let name = unwrap_reference(inner)?;
    let (name, _) = definitions(schema).get_key_value(name)?;
    is_table(&definitions(schema)[name]).then_some((name.as_str(), array))
}

/// What a key accepts, as the **Type** column shows it.
fn type_of(schema: &Value, property: &Value) -> String {
    if let Some(name) = unwrap_reference(property) {
        let definition = &definitions(schema)[name];
        let text = if is_table(definition) {
            "table".to_owned()
        } else {
            format!("`{name}`")
        };
        return link(schema, name, &text);
    }
    let types: Vec<&str> = match property.get("type") {
        None => return "any".to_owned(),
        Some(Value::String(single)) => vec![single.as_str()],
        Some(Value::Array(several)) => several
            .iter()
            .map(|kind| kind.as_str().expect("a type name"))
            .filter(|kind| *kind != "null")
            .collect(),
        Some(other) => panic!("unexpected type {other}"),
    };
    assert_eq!(types.len(), 1, "a key of several types: {property}");
    match types[0] {
        "array" => {
            let items = property.get("items").expect("an array has items");
            if let Some((name, _)) = table_of(schema, property) {
                link(schema, name, "array of tables")
            } else {
                format!("array of {}", type_of(schema, items))
            }
        }
        "object" => {
            let values = property.get("additionalProperties").unwrap_or_else(|| {
                panic!("an inline object without additionalProperties: {property}")
            });
            format!("table of {}", type_of(schema, values))
        }
        "string" => {
            assert!(
                property.get("pattern").is_none() && property.get("enum").is_none(),
                "an inline constrained string: {property}"
            );
            "string".to_owned()
        }
        "boolean" => "boolean".to_owned(),
        "integer" | "number" => number(property, types[0]),
        other => panic!("unexpected type {other} in {property}"),
    }
}

/// `integer, 1–100` from the bounds. The `minimum: 0` schemars gives every
/// unsigned integer goes without saying.
fn number(property: &Value, kind: &str) -> String {
    let unsigned = property
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(|format| format.starts_with("uint"));
    let minimum = property
        .get("minimum")
        .and_then(Value::as_f64)
        .filter(|minimum| !(unsigned && *minimum == 0.0));
    let maximum = property.get("maximum").and_then(Value::as_f64);
    match (minimum, maximum) {
        (Some(low), Some(high)) => format!("{kind}, {low}–{high}"),
        (Some(low), None) => format!("{kind}, ≥ {low}"),
        (None, Some(high)) => format!("{kind}, ≤ {high}"),
        (None, None) => kind.to_owned(),
    }
}

fn render_value_type(page: &mut String, schema: &Value, name: &str, definition: &Value) {
    writeln!(
        page,
        "<a id=\"{}\"></a>\n\n### `{name}`\n\n{}\n",
        anchor(schema, name),
        paragraph(description(definition))
    )
    .unwrap();
    if let Some(choices) = definition.get("oneOf").and_then(Value::as_array) {
        writeln!(page, "| Value | Meaning |").unwrap();
        writeln!(page, "|-------|---------|").unwrap();
        for choice in choices {
            let value = choice
                .get("const")
                .unwrap_or_else(|| panic!("a choice of {name} without const: {choice}"));
            writeln!(
                page,
                "| {} | {} |",
                default(value),
                cell(description(choice))
            )
            .unwrap();
        }
        writeln!(page).unwrap();
        return;
    }
    assert_eq!(
        definition.get("type"),
        Some(&Value::from("string")),
        "unexpected value type {name}: {definition}"
    );
    if let Some(pattern) = definition.get("pattern").and_then(Value::as_str) {
        writeln!(page, "A string matching `{pattern}`.\n").unwrap();
    } else {
        writeln!(page, "A string.\n").unwrap();
    }
}

fn description(value: &Value) -> &str {
    value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Rewraps schemars' line-broken descriptions as one paragraph each.
fn paragraph(text: &str) -> String {
    text.split("\n\n")
        .map(|part| unlink(&part.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Turns a rustdoc intra-doc link, such as "[`Auth`]", into plain code: on
/// this page it would otherwise show its brackets.
fn unlink(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[`") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after[1..].find("`]") {
            Some(end) if !after[end + 3..].starts_with('(') => {
                out.push_str(&after[..end + 2]);
                rest = &after[end + 3..];
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A description inside one table cell.
fn cell(text: &str) -> String {
    paragraph(text)
        .replace("\n\n", "<br><br>")
        .replace('|', "\\|")
}

/// A default as it would be written in TOML.
fn default(value: &Value) -> String {
    let text = match value {
        Value::Null => return String::new(),
        Value::String(text) => format!("{text:?}"),
        Value::Array(items) if items.is_empty() => "[]".to_owned(),
        other => other.to_string(),
    };
    format!("`{}`", text.replace('|', "\\|"))
}
