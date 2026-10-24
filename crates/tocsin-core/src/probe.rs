// SPDX-License-Identifier: AGPL-3.0-only

//! What one probe run observed, and the verdicts its assertions reach.
//!
//! A probe gathers [`Facts`]; [`evaluate`] turns them and a check's
//! assertions into [`AssertionVerdict`]s; [`health_of`] reduces the
//! verdicts to one [`Health`]: up when every assertion passes, otherwise
//! the worst `on_fail` among those that failed. A probe that could not
//! complete (a timeout, a refused connection) has no facts and is down.
//!
//! Nothing here does I/O; the `tocsin-probe` crate gathers the facts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Health;
use regex::Regex;
use serde_json_path::JsonPath;

use crate::config::{
    Assertion, Check, CheckType, Comparison, Condition, FailureState, HeaderAssertion,
    JsonAssertion, RecordType, RecordsAssertion, TextAssertion,
};

/// The longest text an assertion verdict quotes as its actual value.
const QUOTE_LIMIT: usize = 256;

/// The result of running one check once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeOutcome {
    /// The check's id.
    pub check: String,
    /// The check's type.
    #[serde(rename = "type")]
    pub check_type: CheckType,
    /// What was probed.
    pub target: String,
    /// When the probe started, in milliseconds since the Unix epoch.
    pub started_at_ms: i64,
    /// How long the probe took, in milliseconds.
    pub duration_ms: u64,
    /// The check's health on this result.
    pub health: Health,
    /// What the probe observed; absent when it could not complete.
    pub facts: Option<Facts>,
    /// Each assertion's verdict, in configuration order. Empty when the
    /// probe could not complete.
    pub assertions: Vec<AssertionVerdict>,
    /// Why the probe could not complete.
    pub error: Option<ProbeError>,
}

impl ProbeOutcome {
    /// The outcome of a probe that completed with `facts`.
    #[must_use]
    pub fn completed(
        compiled: &CompiledCheck,
        started_at_ms: i64,
        duration_ms: u64,
        facts: Facts,
    ) -> Self {
        let assertions = evaluate(compiled, &facts);
        let check = compiled.check();
        ProbeOutcome {
            check: check.id.to_string(),
            check_type: check.check_type,
            target: check.target.clone(),
            started_at_ms,
            duration_ms,
            health: health_of(&assertions),
            facts: Some(facts),
            assertions,
            error: None,
        }
    }

    /// The outcome of a probe that could not complete: always down.
    #[must_use]
    pub fn failed(check: &Check, started_at_ms: i64, duration_ms: u64, error: ProbeError) -> Self {
        ProbeOutcome {
            check: check.id.to_string(),
            check_type: check.check_type,
            target: check.target.clone(),
            started_at_ms,
            duration_ms,
            health: Health::Down,
            facts: None,
            assertions: Vec::new(),
            error: Some(error),
        }
    }
}

/// Why a probe could not complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeError {
    /// What kind of failure it was.
    pub kind: ProbeErrorKind,
    /// What happened, for people.
    pub message: String,
}

impl ProbeError {
    /// A probe error of `kind`.
    #[must_use]
    pub fn new(kind: ProbeErrorKind, message: impl Into<String>) -> Self {
        ProbeError {
            kind,
            message: message.into(),
        }
    }
}

/// What kind of failure stopped a probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeErrorKind {
    /// The check's timeout passed.
    Timeout,
    /// The connection could not be made: refused, unreachable, or the
    /// host name did not resolve.
    Connect,
    /// The TLS handshake failed, including a certificate that does not
    /// verify.
    Tls,
    /// The DNS lookup itself failed (no answer from the resolver).
    Dns,
    /// The server answered with something that is not the protocol.
    Protocol,
    /// The probe could not be set up on this machine, such as an ICMP
    /// socket the process may not open. This is about tocsin, not the
    /// target.
    Setup,
}

/// What a probe observed, by check type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Facts {
    /// An HTTP response.
    Http(HttpFacts),
    /// A TLS handshake.
    Tls(TlsFacts),
    /// A TCP connection.
    Tcp(TcpFacts),
    /// A DNS lookup.
    Dns(DnsFacts),
    /// An ICMP echo.
    Icmp(IcmpFacts),
}

impl Facts {
    /// The probe's latency, in milliseconds.
    #[must_use]
    pub fn latency_ms(&self) -> u64 {
        match self {
            Facts::Http(facts) => facts.latency_ms,
            Facts::Tls(facts) => facts.latency_ms,
            Facts::Tcp(facts) => facts.latency_ms,
            Facts::Dns(facts) => facts.latency_ms,
            Facts::Icmp(facts) => facts.latency_ms,
        }
    }

    /// The server certificate, for `http` (https only) and `tls`.
    #[must_use]
    pub fn certificate(&self) -> Option<&CertificateFacts> {
        match self {
            Facts::Http(facts) => facts.certificate.as_ref(),
            Facts::Tls(facts) => Some(&facts.certificate),
            Facts::Tcp(_) | Facts::Dns(_) | Facts::Icmp(_) => None,
        }
    }
}

/// What an `http` probe observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpFacts {
    /// From sending the request to reading the body, in milliseconds.
    pub latency_ms: u64,
    /// The URL of the response: the target, or where redirects led.
    pub url: String,
    /// The response status.
    pub status: u16,
    /// The `Location` header of a 3xx response that was not followed.
    pub redirect_location: Option<String>,
    /// Response headers, names in lower case, each with every value sent.
    pub headers: BTreeMap<String, Vec<String>>,
    /// The body as text (invalid UTF-8 replaced), at most the check's
    /// `max_body` of it.
    pub body: String,
    /// How many body bytes were read: the check's `max_body` exactly when
    /// the body was cut.
    pub body_bytes: usize,
    /// Whether the body was longer than the check's `max_body` and cut
    /// short.
    pub body_truncated: bool,
    /// The server certificate, for an https target.
    pub certificate: Option<CertificateFacts>,
}

/// What a `tls` probe observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsFacts {
    /// Connecting and completing the handshake, in milliseconds.
    pub latency_ms: u64,
    /// The server certificate.
    pub certificate: CertificateFacts,
}

/// The certificate chain a TLS server presented, and whether it verifies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateFacts {
    /// The name sent as SNI and checked against the certificate.
    pub server_name: String,
    /// The chain as sent, the server's own certificate first.
    pub chain: Vec<CertificateSummary>,
    /// Seconds until the server's certificate expires; negative once it
    /// has. Absent when the server's certificate could not be parsed.
    pub expires_in_seconds: Option<i64>,
    /// Whole days until the server's certificate expires, rounded down;
    /// negative once it has. Absent when the server's certificate could
    /// not be parsed.
    pub days_remaining: Option<i64>,
    /// Whether the chain leads to a trusted root and every certificate is
    /// within its validity period.
    pub chain_valid: bool,
    /// Why the chain does not verify.
    pub chain_error: Option<String>,
    /// Whether the server's certificate is valid for `server_name`.
    pub hostname_valid: bool,
}

/// One certificate in a chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateSummary {
    /// The subject's distinguished name.
    pub subject: String,
    /// The issuer's distinguished name.
    pub issuer: String,
    /// Valid from, RFC 3339 in UTC.
    pub not_before: String,
    /// Valid until, RFC 3339 in UTC.
    pub not_after: String,
    /// DNS names and IP addresses in the subject alternative names.
    pub subject_alternative_names: Vec<String>,
}

/// What a `tcp` probe observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TcpFacts {
    /// Connecting, in milliseconds.
    pub latency_ms: u64,
    /// The address connected to.
    pub address: String,
    /// What the server sent first (after `send`), up to the first newline;
    /// read only when a `banner` assertion needs it.
    pub banner: Option<String>,
}

/// What a `dns` probe observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsFacts {
    /// The lookup, in milliseconds.
    pub latency_ms: u64,
    /// The resolver asked, as `ip:port`, or `system`.
    pub resolver: String,
    /// The record type asked for.
    pub record_type: RecordType,
    /// The response code, such as `NOERROR` or `NXDOMAIN`.
    pub rcode: String,
    /// The answer records of the type asked for, as text.
    pub answers: Vec<String>,
}

/// What an `icmp` probe observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IcmpFacts {
    /// The echo's round trip, in milliseconds.
    pub latency_ms: u64,
    /// The address pinged.
    pub address: String,
}

/// One assertion's verdict on a result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionVerdict {
    /// The condition's key, such as `status` or `json`.
    pub condition: String,
    /// What the assertion asks for, as configured.
    pub expected: Value,
    /// What the probe found.
    pub actual: Value,
    /// Whether the condition holds.
    pub passed: bool,
    /// The state a failure puts the check in.
    pub on_fail: FailureState,
    /// True for the check type's default condition, applied when the
    /// check has no assertions of its own.
    pub implicit: bool,
    /// Why the condition could not be evaluated, when it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The health a set of verdicts gives: up when every one passed, otherwise
/// the worst `on_fail` among those that failed.
#[must_use]
pub fn health_of(verdicts: &[AssertionVerdict]) -> Health {
    verdicts
        .iter()
        .filter(|verdict| !verdict.passed)
        .map(|verdict| Health::from(verdict.on_fail))
        .max()
        .unwrap_or(Health::Up)
}

/// A check ready to evaluate. Each assertion is compiled once: its
/// JSON path or regular expression parsed and its one matcher chosen, so
/// evaluating never meets one that does not parse.
#[derive(Debug, Clone)]
pub struct CompiledCheck {
    check: Check,
    rules: Vec<Rule>,
}

impl CompiledCheck {
    /// Compiles `check`'s assertions.
    ///
    /// # Errors
    ///
    /// Why an assertion does not compile: it does not hold exactly one
    /// condition with exactly one matcher, or its JSON path or regular
    /// expression does not parse. `config::load` reports each of these,
    /// with its location, first, so every check of a loaded configuration
    /// compiles.
    pub fn new(check: Check) -> Result<Self, String> {
        let rules = check
            .assertions
            .iter()
            .enumerate()
            .map(|(index, assertion)| {
                Rule::new(assertion).map_err(|error| {
                    format!("check `{}`, assertion {}: {error}", check.id, index + 1)
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(CompiledCheck { check, rules })
    }

    /// The check as configured.
    #[must_use]
    pub fn check(&self) -> &Check {
        &self.check
    }
}

/// Parses an RFC 9535 JSON path, explaining why it does not parse.
pub(crate) fn json_path(path: &str) -> Result<JsonPath, String> {
    JsonPath::parse(path).map_err(|error| format!("`{path}` is not a JSONPath: {error}"))
}

/// Compiles a regular expression, explaining why it does not compile.
pub(crate) fn regular_expression(pattern: &str) -> Result<Regex, String> {
    Regex::new(pattern).map_err(|error| format!("`{pattern}` is not a regular expression: {error}"))
}

/// One compiled assertion.
#[derive(Debug, Clone)]
struct Rule {
    key: &'static str,
    expected: Value,
    on_fail: FailureState,
    test: Test,
}

/// What a compiled assertion tests.
#[derive(Debug, Clone)]
enum Test {
    Status(Vec<u16>),
    LatencyMilliseconds(Comparison),
    Json(JsonPath, JsonTest),
    Body(TextTest),
    Header(HeaderAssertion),
    CertificateDaysRemaining(Comparison),
    HostnameValid(bool),
    ChainValid(bool),
    Records(RecordsAssertion),
    Banner(TextTest),
}

/// A `json` assertion's one matcher.
#[derive(Debug, Clone)]
enum JsonTest {
    Eq(Value),
    Ne(Value),
    Exists(bool),
    Contains(String),
}

/// A `body` or `banner` assertion's one matcher.
#[derive(Debug, Clone)]
enum TextTest {
    Contains(String),
    NotContains(String),
    Regex(Regex),
}

impl Rule {
    fn new(assertion: &Assertion) -> Result<Self, String> {
        let conditions = assertion.conditions();
        let [condition] = conditions.as_slice() else {
            return Err(format!(
                "an assertion holds one condition, and this one has {}",
                conditions.len()
            ));
        };
        let (expected, test) = match *condition {
            Condition::Status(codes) => (json!(codes), Test::Status(codes.to_vec())),
            Condition::LatencyMilliseconds(comparison) => {
                (json!(comparison), Test::LatencyMilliseconds(*comparison))
            }
            Condition::Json(json) => (
                json!(json),
                Test::Json(json_path(&json.path)?, JsonTest::new(json)?),
            ),
            Condition::Body(text) => (json!(text), Test::Body(TextTest::new(text)?)),
            Condition::Header(header) => (json!(header), Test::Header(header.clone())),
            Condition::CertificateDaysRemaining(comparison) => (
                json!(comparison),
                Test::CertificateDaysRemaining(*comparison),
            ),
            Condition::HostnameValid(wanted) => (json!(wanted), Test::HostnameValid(wanted)),
            Condition::ChainValid(wanted) => (json!(wanted), Test::ChainValid(wanted)),
            Condition::Records(records) => (json!(records), Test::Records(records.clone())),
            Condition::Banner(text) => (json!(text), Test::Banner(TextTest::new(text)?)),
        };
        Ok(Rule {
            key: condition.key(),
            expected,
            on_fail: assertion.on_fail,
            test,
        })
    }
}

impl JsonTest {
    fn new(json: &JsonAssertion) -> Result<Self, String> {
        match (&json.eq, &json.ne, json.exists, &json.contains) {
            (Some(wanted), None, None, None) => Ok(JsonTest::Eq(wanted.clone())),
            (None, Some(unwanted), None, None) => Ok(JsonTest::Ne(unwanted.clone())),
            (None, None, Some(wanted), None) => Ok(JsonTest::Exists(wanted)),
            (None, None, None, Some(needle)) => Ok(JsonTest::Contains(needle.clone())),
            _ => Err("give exactly one of eq, ne, exists or contains".to_owned()),
        }
    }
}

impl TextTest {
    fn new(text: &TextAssertion) -> Result<Self, String> {
        match (&text.contains, &text.not_contains, &text.regex) {
            (Some(needle), None, None) => Ok(TextTest::Contains(needle.clone())),
            (None, Some(needle), None) => Ok(TextTest::NotContains(needle.clone())),
            (None, None, Some(pattern)) => regular_expression(pattern).map(TextTest::Regex),
            _ => Err("give exactly one of contains, not_contains or regex".to_owned()),
        }
    }
}

/// Every assertion's verdict on `facts`. A check with no assertions gets
/// its type's implicit ones: a 2xx or 3xx status for `http`, `NOERROR` for
/// `dns`, a certificate that verifies for the host for `tls`, and none
/// (completing is enough) for `tcp` and `icmp`.
#[must_use]
pub fn evaluate(check: &CompiledCheck, facts: &Facts) -> Vec<AssertionVerdict> {
    if check.rules.is_empty() {
        return implicit(facts);
    }
    check
        .rules
        .iter()
        .map(|rule| {
            let Evaluation {
                actual,
                passed,
                message,
            } = rule.test.evaluate(facts);
            AssertionVerdict {
                condition: rule.key.to_owned(),
                expected: rule.expected.clone(),
                actual,
                passed,
                on_fail: rule.on_fail,
                implicit: false,
                message,
            }
        })
        .collect()
}

struct Evaluation {
    actual: Value,
    passed: bool,
    message: Option<String>,
}

impl Evaluation {
    fn new(actual: Value, passed: bool) -> Self {
        Evaluation {
            actual,
            passed,
            message: None,
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Evaluation {
            actual: Value::Null,
            passed: false,
            message: Some(message.into()),
        }
    }
}

fn implicit(facts: &Facts) -> Vec<AssertionVerdict> {
    let verdict =
        |condition: &str, expected: Value, actual: Value, passed: bool| AssertionVerdict {
            condition: condition.to_owned(),
            expected,
            actual,
            passed,
            on_fail: FailureState::Down,
            implicit: true,
            message: None,
        };
    match facts {
        Facts::Http(http) => vec![verdict(
            "status",
            json!("2xx or 3xx"),
            json!(http.status),
            (200..400).contains(&http.status),
        )],
        Facts::Dns(dns) => vec![verdict(
            "rcode",
            json!("NOERROR"),
            json!(dns.rcode),
            dns.rcode == "NOERROR",
        )],
        Facts::Tls(tls) => vec![
            verdict(
                "chain_valid",
                json!(true),
                json!(tls.certificate.chain_valid),
                tls.certificate.chain_valid,
            ),
            verdict(
                "hostname_valid",
                json!(true),
                json!(tls.certificate.hostname_valid),
                tls.certificate.hostname_valid,
            ),
        ],
        Facts::Tcp(_) | Facts::Icmp(_) => Vec::new(),
    }
}

impl Test {
    fn evaluate(&self, facts: &Facts) -> Evaluation {
        match self {
            Test::Status(codes) => match facts {
                Facts::Http(http) => {
                    Evaluation::new(json!(http.status), codes.contains(&http.status))
                }
                _ => Evaluation::unavailable("the result has no HTTP status"),
            },
            Test::LatencyMilliseconds(comparison) => {
                let latency = facts.latency_ms();
                Evaluation::new(
                    json!(latency),
                    comparison.holds(i64::try_from(latency).unwrap_or(i64::MAX)),
                )
            }
            Test::Json(path, test) => match facts {
                Facts::Http(http) => evaluate_json(path, test, http),
                _ => Evaluation::unavailable("the result has no response body"),
            },
            Test::Body(text) => match facts {
                Facts::Http(http) => evaluate_text(text, Some(&http.body)),
                _ => Evaluation::unavailable("the result has no response body"),
            },
            Test::Banner(text) => match facts {
                Facts::Tcp(tcp) => evaluate_text(text, tcp.banner.as_deref()),
                _ => Evaluation::unavailable("the result has no banner"),
            },
            Test::Header(header) => match facts {
                Facts::Http(http) => evaluate_header(header, http),
                _ => Evaluation::unavailable("the result has no headers"),
            },
            Test::CertificateDaysRemaining(comparison) => {
                certificate(facts, |certificate| match certificate.days_remaining {
                    Some(days) => Evaluation::new(json!(days), comparison.holds(days)),
                    None => Evaluation::unavailable(
                        "the certificate's expiry is unavailable: the server's certificate could not be parsed",
                    ),
                })
            }
            Test::HostnameValid(wanted) => certificate(facts, |certificate| {
                Evaluation::new(
                    json!(certificate.hostname_valid),
                    certificate.hostname_valid == *wanted,
                )
            }),
            Test::ChainValid(wanted) => certificate(facts, |certificate| {
                Evaluation::new(
                    json!(certificate.chain_valid),
                    certificate.chain_valid == *wanted,
                )
            }),
            Test::Records(records) => match facts {
                Facts::Dns(dns) => evaluate_records(records, dns.record_type, &dns.answers),
                _ => Evaluation::unavailable("the result has no DNS answers"),
            },
        }
    }
}

fn certificate(facts: &Facts, test: impl FnOnce(&CertificateFacts) -> Evaluation) -> Evaluation {
    match facts.certificate() {
        Some(certificate) => test(certificate),
        None => Evaluation::unavailable("the result has no certificate"),
    }
}

/// `eq`, `ne` and `contains` test the first value the path selects, in
/// document order; `exists` tests whether it selects any. A path that
/// selects nothing fails every matcher but `exists = false`.
fn evaluate_json(path: &JsonPath, test: &JsonTest, http: &HttpFacts) -> Evaluation {
    let document: Value = match serde_json::from_str(&http.body) {
        Ok(document) => document,
        Err(error) => {
            // A cut body is exactly `max_body` long.
            let truncated = if http.body_truncated {
                format!(
                    " (the body was cut at {} bytes, the check's `max_body`; raise it in `[check.http]` to read more)",
                    http.body_bytes
                )
            } else {
                String::new()
            };
            return Evaluation::unavailable(format!("the body is not JSON: {error}{truncated}"));
        }
    };
    let nodes = path.query(&document);
    match (test, nodes.first()) {
        (JsonTest::Exists(wanted), first) => {
            let found = first.is_some();
            Evaluation::new(json!(found), found == *wanted)
        }
        (_, None) => Evaluation::unavailable("the path selects nothing"),
        (JsonTest::Eq(wanted), Some(first)) => Evaluation::new((*first).clone(), *first == *wanted),
        (JsonTest::Ne(unwanted), Some(first)) => {
            Evaluation::new((*first).clone(), *first != *unwanted)
        }
        (JsonTest::Contains(needle), Some(first)) => Evaluation::new(
            (*first).clone(),
            first
                .as_str()
                .is_some_and(|text| text.contains(needle.as_str())),
        ),
    }
}

fn evaluate_text(matcher: &TextTest, text: Option<&str>) -> Evaluation {
    let Some(text) = text else {
        return Evaluation::unavailable("nothing was received");
    };
    let passed = match matcher {
        TextTest::Contains(needle) => text.contains(needle.as_str()),
        TextTest::NotContains(needle) => !text.contains(needle.as_str()),
        TextTest::Regex(regex) => regex.is_match(text),
    };
    Evaluation::new(json!(quote(text)), passed)
}

fn evaluate_header(assertion: &HeaderAssertion, http: &HttpFacts) -> Evaluation {
    let values = http.headers.get(&assertion.name.to_ascii_lowercase());
    let actual = values.map_or(Value::Null, |values| json!(values));
    let any = |test: &dyn Fn(&str) -> bool| {
        values.is_some_and(|values| values.iter().any(|value| test(value)))
    };
    let passed = if let Some(wanted) = &assertion.eq {
        any(&|value| value == wanted)
    } else if let Some(needle) = &assertion.contains {
        any(&|value| value.contains(needle.as_str()))
    } else if let Some(wanted) = assertion.exists {
        values.is_some() == wanted
    } else {
        false
    };
    Evaluation::new(actual, passed)
}

fn evaluate_records(
    assertion: &RecordsAssertion,
    record_type: RecordType,
    answers: &[String],
) -> Evaluation {
    let normalize = |record: &String| normalize_record(record, record_type);
    let found: Vec<String> = answers.iter().map(normalize).collect();
    let passed = if let Some(wanted) = &assertion.contains {
        found.contains(&normalize(wanted))
    } else if let Some(wanted) = &assertion.eq {
        let mut wanted: Vec<String> = wanted.iter().map(normalize).collect();
        let mut found = found.clone();
        wanted.sort();
        wanted.dedup();
        found.sort();
        found.dedup();
        wanted == found
    } else {
        false
    };
    Evaluation::new(json!(answers), passed)
}

/// A record as compared: an IP address in its canonical form, anything
/// else without a trailing `.`. Records that hold a domain name (CNAME,
/// MX, NS and SRV) are lowercased, since names match without regard to
/// case; TXT and CAA text keeps its case.
fn normalize_record(record: &str, record_type: RecordType) -> String {
    let record = record.trim();
    if let Ok(address) = record.parse::<std::net::IpAddr>() {
        return address.to_string();
    }
    let record = record.strip_suffix('.').unwrap_or(record);
    match record_type {
        RecordType::Cname | RecordType::Mx | RecordType::Ns | RecordType::Srv => {
            record.to_ascii_lowercase()
        }
        RecordType::A | RecordType::Aaaa | RecordType::Txt | RecordType::Caa => record.to_owned(),
    }
}

/// `text`, cut to [`QUOTE_LIMIT`] characters with a trailing `…` when
/// longer.
fn quote(text: &str) -> String {
    match text.char_indices().nth(QUOTE_LIMIT) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// Formats seconds since the Unix epoch as RFC 3339 in UTC, such as
/// `2026-10-23T00:00:00Z`.
#[must_use]
pub fn rfc3339(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let seconds = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01, from Howard
/// Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(toml: &str) -> CompiledCheck {
        CompiledCheck::new(toml::from_str(toml).expect("a valid check"))
            .expect("a check that compiles")
    }

    fn http(status: u16, body: &str) -> Facts {
        Facts::Http(HttpFacts {
            latency_ms: 120,
            url: "https://example.com/".into(),
            status,
            redirect_location: None,
            headers: BTreeMap::from([(
                "content-type".to_owned(),
                vec!["application/json".to_owned()],
            )]),
            body: body.to_owned(),
            body_bytes: body.len(),
            body_truncated: false,
            certificate: None,
        })
    }

    /// The body of an Atlassian Statuspage `/api/v2/components.json`,
    /// trimmed to the fields a check reads.
    const STATUSPAGE_COMPONENTS: &str = r#"{
      "page": {"id": "kctbh9vrtdwd", "name": "GitHub", "url": "https://www.githubstatus.com"},
      "components": [
        {"id": "8l4ygp009s5s", "name": "Git Operations", "status": "operational", "position": 1, "group": false},
        {"id": "brv1bkgrwx7q", "name": "API Requests", "status": "degraded_performance", "position": 2, "group": false},
        {"id": "4230lsnqdsld", "name": "Webhooks", "status": "operational", "position": 3, "group": false},
        {"id": "0l2p9nhqnxpd", "name": "Visit www.githubstatus.com for more information", "status": "operational", "position": 4, "group": false},
        {"id": "kr09ddfgbfsf", "name": "Issues", "status": "major_outage", "position": 5, "group": false}
      ]
    }"#;

    fn statuspage_check(component: &str, on_fail: &str) -> CompiledCheck {
        check(&format!(
            r#"
            id = "github"
            type = "http"
            target = "https://www.githubstatus.com/api/v2/components.json"
            [[assert]]
            json = {{ path = "$.components[?@.name=='{component}'].status", eq = "operational" }}
            on_fail = "{on_fail}"
            "#
        ))
    }

    /// Gatus' `[BODY].components.#(name=="X").status == operational`
    /// ports one-to-one to an RFC 9535 filter expression.
    #[test]
    fn statuspage_component_filters() {
        let facts = http(200, STATUSPAGE_COMPONENTS);

        let up = evaluate(&statuspage_check("Git Operations", "down"), &facts);
        assert!(up[0].passed);
        assert_eq!(up[0].actual, json!("operational"));
        assert_eq!(health_of(&up), Health::Up);

        let degraded = evaluate(&statuspage_check("API Requests", "degraded"), &facts);
        assert!(!degraded[0].passed);
        assert_eq!(degraded[0].actual, json!("degraded_performance"));
        assert_eq!(health_of(&degraded), Health::Degraded);

        let down = evaluate(&statuspage_check("Issues", "down"), &facts);
        assert_eq!(down[0].actual, json!("major_outage"));
        assert_eq!(health_of(&down), Health::Down);

        let missing = evaluate(&statuspage_check("Pages", "down"), &facts);
        assert!(!missing[0].passed, "a component that is gone fails");
        assert_eq!(
            missing[0].message.as_deref(),
            Some("the path selects nothing")
        );
    }

    #[test]
    fn json_matchers() {
        let facts = http(
            200,
            r#"{"checks": [{"ok": true, "name": "db"}, {"ok": false}]}"#,
        );
        let cases = [
            (r#"json = { path = "$.checks[0].ok", eq = true }"#, true),
            (r#"json = { path = "$.checks[1].ok", eq = true }"#, false),
            (r#"json = { path = "$.checks[1].ok", ne = true }"#, true),
            (r#"json = { path = "$.checks[*].ok", eq = true }"#, true),
            (
                r#"json = { path = "$.checks[0].name", contains = "d" }"#,
                true,
            ),
            (
                r#"json = { path = "$.checks[0].ok", contains = "t" }"#,
                false,
            ),
            (r#"json = { path = "$.nope", exists = false }"#, true),
            (
                r#"json = { path = "$.checks[?@.ok == false]", exists = true }"#,
                true,
            ),
        ];
        for (assertion, passed) in cases {
            let check = check(&format!(
                "id = \"a\"\ntype = \"http\"\ntarget = \"https://a.example\"\n[[assert]]\n{assertion}"
            ));
            assert_eq!(evaluate(&check, &facts)[0].passed, passed, "{assertion}");
        }
        let not_json = check(
            "id = \"a\"\ntype = \"http\"\ntarget = \"https://a.example\"\n[[assert]]\njson = { path = \"$.a\", exists = true }",
        );
        let verdict = &evaluate(&not_json, &http(200, "<html>"))[0];
        assert!(!verdict.passed);
        assert!(
            verdict
                .message
                .as_deref()
                .unwrap()
                .starts_with("the body is not JSON")
        );
    }

    #[test]
    fn worst_failure_wins() {
        let verdict = |passed, on_fail| AssertionVerdict {
            condition: "status".into(),
            expected: Value::Null,
            actual: Value::Null,
            passed,
            on_fail,
            implicit: false,
            message: None,
        };
        assert_eq!(health_of(&[]), Health::Up);
        assert_eq!(
            health_of(&[
                verdict(true, FailureState::Down),
                verdict(true, FailureState::Degraded)
            ]),
            Health::Up
        );
        assert_eq!(
            health_of(&[
                verdict(true, FailureState::Down),
                verdict(false, FailureState::Degraded)
            ]),
            Health::Degraded
        );
        assert_eq!(
            health_of(&[
                verdict(false, FailureState::Degraded),
                verdict(false, FailureState::Down),
                verdict(false, FailureState::Degraded),
            ]),
            Health::Down
        );
    }

    #[test]
    fn implicit_verdicts() {
        let bare = check("id = \"a\"\ntype = \"http\"\ntarget = \"https://a.example\"");
        for (status, passed) in [
            (200, true),
            (204, true),
            (301, true),
            (399, true),
            (404, false),
            (503, false),
            (199, false),
        ] {
            let verdicts = evaluate(&bare, &http(status, ""));
            assert_eq!(verdicts.len(), 1);
            assert!(verdicts[0].implicit);
            assert_eq!(verdicts[0].passed, passed, "{status}");
        }
        let tcp = check("id = \"a\"\ntype = \"tcp\"\ntarget = \"a.example:22\"");
        let facts = Facts::Tcp(TcpFacts {
            latency_ms: 1,
            address: "192.0.2.1:22".into(),
            banner: None,
        });
        assert_eq!(evaluate(&tcp, &facts).len(), 0);
    }

    #[test]
    fn text_and_header_matchers() {
        let facts = http(200, "SSH-2.0-OpenSSH_9.6 ready");
        let body = |assertion: &str| {
            let check = check(&format!(
                "id = \"a\"\ntype = \"http\"\ntarget = \"https://a.example\"\n[[assert]]\n{assertion}"
            ));
            evaluate(&check, &facts)[0].passed
        };
        assert!(body(r#"body = { contains = "OpenSSH" }"#));
        assert!(!body(r#"body = { not_contains = "OpenSSH" }"#));
        assert!(body(r"body = { regex = '^SSH-2\.0-\S+' }"));
        assert!(!body(r"body = { regex = '^OpenSSH' }"));
        assert!(body(
            r#"header = { name = "Content-Type", contains = "json" }"#
        ));
        assert!(body(
            r#"header = { name = "content-type", eq = "application/json" }"#
        ));
        assert!(!body(r#"header = { name = "x-missing", exists = true }"#));
        assert!(body(r#"header = { name = "x-missing", exists = false }"#));
    }

    #[test]
    fn records_and_certificates() {
        let dns = Facts::Dns(DnsFacts {
            latency_ms: 3,
            resolver: "192.0.2.53:53".into(),
            record_type: RecordType::Cname,
            rcode: "NOERROR".into(),
            answers: vec!["edge.example.net.".into(), "2001:db8::1".into()],
        });
        let records = |assertion: &str| {
            let check = check(&format!(
                "id = \"a\"\ntype = \"dns\"\ntarget = \"a.example\"\n[[assert]]\n{assertion}"
            ));
            evaluate(&check, &dns)[0].passed
        };
        assert!(records(r#"records = { contains = "edge.example.net" }"#));
        assert!(records(r#"records = { contains = "2001:0db8:0:0::1" }"#));
        assert!(records(
            r#"records = { eq = ["2001:db8::1", "edge.example.net."] }"#
        ));
        assert!(!records(r#"records = { eq = ["edge.example.net"] }"#));

        let certificate = CertificateFacts {
            server_name: "a.example".into(),
            chain: Vec::new(),
            expires_in_seconds: Some(10 * 86_400),
            days_remaining: Some(10),
            chain_valid: true,
            chain_error: None,
            hostname_valid: false,
        };
        let tls = Facts::Tls(TlsFacts {
            latency_ms: 5,
            certificate,
        });
        let verdicts = evaluate(
            &check(
                "id = \"a\"\ntype = \"tls\"\ntarget = \"a.example\"\n\
                 [[assert]]\ncertificate_days_remaining = { gt = 14 }\non_fail = \"degraded\"\n\
                 [[assert]]\nhostname_valid = true\n\
                 [[assert]]\nchain_valid = true",
            ),
            &tls,
        );
        let passed: Vec<_> = verdicts.iter().map(|verdict| verdict.passed).collect();
        assert_eq!(passed, [false, false, true]);
        assert_eq!(health_of(&verdicts), Health::Down);
        assert_eq!(health_of(&implicit(&tls)), Health::Down);
    }

    /// Names match without regard to case; TXT keeps its case.
    #[test]
    fn name_records_ignore_case() {
        let answers = |record_type: RecordType, answers: &[&str]| {
            Facts::Dns(DnsFacts {
                latency_ms: 3,
                resolver: "192.0.2.53:53".into(),
                record_type,
                rcode: "NOERROR".into(),
                answers: answers.iter().map(|answer| (*answer).to_owned()).collect(),
            })
        };
        let passes = |record_type: &str, assertion: &str, facts: &Facts| {
            let check = check(&format!(
                "id = \"a\"\ntype = \"dns\"\ntarget = \"a.example\"\n\
                 [dns]\nrecord_type = \"{record_type}\"\n[[assert]]\n{assertion}"
            ));
            evaluate(&check, facts)[0].passed
        };
        let cname = answers(RecordType::Cname, &["Edge.Example.NET."]);
        assert!(passes(
            "CNAME",
            r#"records = { contains = "edge.example.net" }"#,
            &cname
        ));
        assert!(passes(
            "CNAME",
            r#"records = { eq = ["EDGE.example.net"] }"#,
            &cname
        ));
        let mx = answers(RecordType::Mx, &["10 Mail.Example.com."]);
        assert!(passes(
            "MX",
            r#"records = { contains = "10 mail.example.com" }"#,
            &mx
        ));
        let ns = answers(RecordType::Ns, &["NS1.Example.com."]);
        assert!(passes(
            "NS",
            r#"records = { contains = "ns1.example.com" }"#,
            &ns
        ));
        let srv = answers(RecordType::Srv, &["10 5 5060 SIP.Example.com."]);
        assert!(passes(
            "SRV",
            r#"records = { contains = "10 5 5060 sip.example.com" }"#,
            &srv
        ));
        let txt = answers(RecordType::Txt, &["v=spf1 -ALL"]);
        assert!(passes(
            "TXT",
            r#"records = { contains = "v=spf1 -ALL" }"#,
            &txt
        ));
        assert!(!passes(
            "TXT",
            r#"records = { contains = "v=spf1 -all" }"#,
            &txt
        ));
    }

    /// A certificate whose expiry could not be read fails its
    /// `certificate_days_remaining` assertion as unavailable, with its
    /// `on_fail`, rather than reading as zero days.
    #[test]
    fn an_unreadable_expiry_is_unavailable() {
        let tls = Facts::Tls(TlsFacts {
            latency_ms: 5,
            certificate: CertificateFacts {
                server_name: "a.example".into(),
                chain: Vec::new(),
                expires_in_seconds: None,
                days_remaining: None,
                chain_valid: true,
                chain_error: None,
                hostname_valid: true,
            },
        });
        let verdicts = evaluate(
            &check(
                "id = \"a\"\ntype = \"tls\"\ntarget = \"a.example\"\n\
                 [[assert]]\ncertificate_days_remaining = { lt = 1 }\non_fail = \"degraded\"",
            ),
            &tls,
        );
        assert!(!verdicts[0].passed);
        assert_eq!(verdicts[0].actual, Value::Null);
        assert!(
            verdicts[0]
                .message
                .as_deref()
                .unwrap()
                .contains("unavailable"),
            "{verdicts:?}"
        );
        assert_eq!(health_of(&verdicts), Health::Degraded);
    }

    /// Compiling reports what `config::load` would; a loaded check always
    /// compiles.
    #[test]
    fn checks_that_do_not_compile() {
        let compile = |assertion: &str| {
            let check: Check = toml::from_str(&format!(
                "id = \"a\"\ntype = \"http\"\ntarget = \"https://a.example\"\n[[assert]]\n{assertion}"
            ))
            .unwrap();
            CompiledCheck::new(check).map(|_| ()).unwrap_err()
        };
        assert_eq!(
            compile(r#"body = { regex = "(unclosed" }"#),
            "check `a`, assertion 1: `(unclosed` is not a regular expression: regex parse error:\n    (unclosed\n    ^\nerror: unclosed group"
        );
        assert!(
            compile(r#"json = { path = "$.a[", eq = 1 }"#).contains("`$.a[` is not a JSONPath")
        );
        assert_eq!(
            compile(r#"json = { path = "$.a", eq = 1, ne = 2 }"#),
            "check `a`, assertion 1: give exactly one of eq, ne, exists or contains"
        );
        assert_eq!(
            compile(r#"banner = { contains = "a", regex = "a" }"#),
            "check `a`, assertion 1: give exactly one of contains, not_contains or regex"
        );
    }

    #[test]
    fn failure_states_map_to_health() {
        assert_eq!(Health::from(FailureState::Degraded), Health::Degraded);
        assert_eq!(Health::from(FailureState::Down), Health::Down);
        let assertion: Assertion = toml::from_str("status = [200]").unwrap();
        assert_eq!(
            assertion.on_fail,
            FailureState::Down,
            "on_fail defaults to down"
        );
    }

    #[test]
    fn dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_792_713_599), "2026-10-22T23:59:59Z");
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn outcomes_serialize() {
        let check = check("id = \"a\"\ntype = \"http\"\ntarget = \"https://a.example\"");
        let outcome = ProbeOutcome::failed(
            check.check(),
            0,
            10_000,
            ProbeError::new(ProbeErrorKind::Timeout, "no response within 10s"),
        );
        let value = serde_json::to_value(&outcome).unwrap();
        assert_eq!(value["health"], json!("down"));
        assert_eq!(value["error"]["kind"], json!("timeout"));
        let back: ProbeOutcome = serde_json::from_value(value).unwrap();
        assert_eq!(back, outcome);
    }
}
