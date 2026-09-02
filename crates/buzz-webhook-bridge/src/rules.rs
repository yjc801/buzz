//! Rule model: the JSON shape, validation, event matching, and template
//! expansion.
//!
//! Rules arrive as JSON (inline via `BRIDGE_RULES` or from
//! `BRIDGE_RULES_FILE` — see [`crate::config`]) and are validated into
//! [`Rule`]s at startup. A malformed rule fails startup loudly; nothing here
//! is repaired or defaulted silently.
//!
//! # Secret hygiene
//!
//! `${VAR}` environment expansion is performed in webhook **header values**
//! and in the **url** only — that is where secrets (bearer tokens, signed
//! URLs) live. The expanded value is held in an [`Expanded`], whose `Debug`
//! and `Display` render the unexpanded template; log the rule, never the
//! revealed string.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;
use serde_json::Value;

/// The per-rule dispatch budget applied when a rule does not set
/// `max_per_minute` — see [`crate::dispatch::TokenBucket`].
pub const DEFAULT_MAX_PER_MINUTE: u32 = 6;

/// Errors turning raw rules JSON into validated [`Rule`]s.
#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    /// The rules document is not the expected JSON shape.
    #[error("rules JSON does not parse: {0}")]
    Json(#[from] serde_json::Error),
    /// One rule failed validation; `rule` names it (or its array index when
    /// the name itself is unusable).
    #[error("rule {rule}: {reason}")]
    Invalid {
        /// The rule's `name`, or `#<index>` when the name is empty.
        rule: String,
        /// What was wrong with it.
        reason: String,
    },
    /// A `${VAR}` reference in a url or header value names an environment
    /// variable that is not set. Startup fails loudly rather than shipping a
    /// webhook with a hole where its secret should be.
    #[error("rule {rule}: ${{{var}}} is referenced but the environment variable is not set")]
    MissingEnvVar {
        /// The rule the reference appears in.
        rule: String,
        /// The unset variable's name.
        var: String,
    },
}

/// A string template whose `${VAR}` environment references have been
/// expanded.
///
/// `Debug` and `Display` render the **unexpanded template**, never the
/// expanded value — `${VAR}` is exactly where secrets live, and a rule that
/// reaches a log line must not take its secrets with it. Reaching the real
/// value takes the explicit [`Expanded::reveal`] call.
#[derive(Clone)]
pub struct Expanded {
    template: String,
    expanded: String,
}

impl Expanded {
    /// The expanded value, secrets included. Use it to build the HTTP
    /// request; never format it into a log line or an error message.
    #[must_use]
    pub fn reveal(&self) -> &str {
        &self.expanded
    }

    /// The unexpanded template — safe to log.
    #[must_use]
    pub fn template(&self) -> &str {
        &self.template
    }
}

impl fmt::Debug for Expanded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.template)
    }
}

impl fmt::Display for Expanded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.template)
    }
}

/// Why a `${VAR}` expansion failed.
#[derive(Debug, PartialEq, Eq)]
enum ExpandError {
    /// The named variable is not set in the environment.
    Unset(String),
    /// A `${` with no closing `}`.
    Unterminated,
}

/// Expand every `${VAR}` reference in `template` from `env`.
///
/// Only the `${NAME}` form is recognized; a bare `$NAME` passes through
/// untouched. A reference to an unset variable is an error, not an empty
/// string — a webhook with a hole where its bearer token should be must not
/// start.
fn expand_env(template: &str, env: &HashMap<String, String>) -> Result<Expanded, ExpandError> {
    let mut expanded = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(ExpandError::Unterminated);
        };
        let name = &after[..end];
        match env.get(name) {
            Some(value) => expanded.push_str(value),
            None => return Err(ExpandError::Unset(name.to_string())),
        }
        rest = &after[end + 1..];
    }
    expanded.push_str(rest);
    Ok(Expanded {
        template: template.to_string(),
        expanded,
    })
}

// ---------------------------------------------------------------------------
// Raw (wire) shape
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    name: String,
    filter: RawFilter,
    webhook: RawWebhook,
    #[serde(default)]
    max_per_minute: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFilter {
    kinds: Vec<u32>,
    authors: Vec<String>,
    #[serde(default)]
    d_prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWebhook {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    body: Option<Value>,
}

// ---------------------------------------------------------------------------
// Validated shape
// ---------------------------------------------------------------------------

/// One validated bridge rule: a relay-side filter and the webhook it fires.
#[derive(Debug, Clone)]
pub struct Rule {
    /// The rule's name — the identifier every log line about this rule
    /// carries.
    pub name: String,
    /// What events this rule reacts to.
    pub filter: RuleFilter,
    /// The webhook it fires on a match.
    pub webhook: Webhook,
    /// This rule's dispatch budget, calls per minute
    /// ([`DEFAULT_MAX_PER_MINUTE`] when unset).
    pub max_per_minute: u32,
}

/// The event filter half of a [`Rule`].
#[derive(Debug, Clone)]
pub struct RuleFilter {
    /// Event kinds, sent in the REQ filter. Non-empty by validation: the
    /// relay p-gates kind-less filters (403), and an unbounded rule is
    /// almost certainly a mistake anyway.
    pub kinds: Vec<u32>,
    /// Author pubkeys (lowercase 64-hex), sent in the REQ filter. Non-empty
    /// by validation — the authors pin is one of the two loop guards the
    /// crate docs name, so a rule cannot opt out of it.
    pub authors: Vec<String>,
    /// Optional prefix the event's `d` tag must start with. Matched
    /// client-side: the relay's `#d` filter is exact-match only.
    pub d_prefix: Option<String>,
}

/// The webhook half of a [`Rule`], env-expanded and validated.
#[derive(Debug, Clone)]
pub struct Webhook {
    /// Request URL. Env-expanded (may hold secrets — see [`Expanded`]);
    /// event placeholders are substituted at dispatch time.
    pub url: Expanded,
    /// HTTP method, default `POST`.
    pub method: reqwest::Method,
    /// Header name/value pairs. Values are env-expanded (this is where
    /// `Authorization: Bearer ${TOKEN}` lives).
    pub headers: Vec<(String, Expanded)>,
    /// Optional JSON body template. Event placeholders are substituted into
    /// its string values at dispatch time; no env expansion happens here.
    pub body: Option<Value>,
}

fn invalid(rule: &str, reason: impl Into<String>) -> RuleError {
    RuleError::Invalid {
        rule: rule.to_string(),
        reason: reason.into(),
    }
}

fn validate_author(author: &str) -> Result<String, String> {
    let normalized = author.trim().to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "author {author:?} is not a 64-character hex pubkey"
        ));
    }
    Ok(normalized)
}

fn expand_for(
    rule: &str,
    template: &str,
    env: &HashMap<String, String>,
) -> Result<Expanded, RuleError> {
    expand_env(template, env).map_err(|error| match error {
        ExpandError::Unset(var) => RuleError::MissingEnvVar {
            rule: rule.to_string(),
            var,
        },
        ExpandError::Unterminated => invalid(rule, "unterminated ${...} reference"),
    })
}

fn validate_rule(
    index: usize,
    raw: RawRule,
    env: &HashMap<String, String>,
) -> Result<Rule, RuleError> {
    let name = raw.name.trim().to_string();
    if name.is_empty() {
        return Err(invalid(&format!("#{index}"), "name must not be empty"));
    }

    if raw.filter.kinds.is_empty() {
        return Err(invalid(
            &name,
            "filter.kinds must list at least one kind (the relay refuses kind-less filters)",
        ));
    }
    if raw.filter.authors.is_empty() {
        return Err(invalid(
            &name,
            "filter.authors must pin at least one author pubkey (the authors pin is a loop guard, \
             not an optimization)",
        ));
    }
    let authors = raw
        .filter
        .authors
        .iter()
        .map(|author| validate_author(author).map_err(|reason| invalid(&name, reason)))
        .collect::<Result<Vec<_>, _>>()?;

    let method = match raw.webhook.method.as_deref() {
        None => reqwest::Method::POST,
        Some(raw_method) => {
            reqwest::Method::from_bytes(raw_method.trim().to_ascii_uppercase().as_bytes())
                .map_err(|_| invalid(&name, format!("invalid webhook.method {raw_method:?}")))?
        }
    };

    let url = expand_for(&name, raw.webhook.url.trim(), env)?;
    let mut headers = Vec::new();
    for (header_name, header_value) in raw.webhook.headers.unwrap_or_default() {
        if header_name.trim().is_empty() {
            return Err(invalid(&name, "a webhook header has an empty name"));
        }
        headers.push((header_name.clone(), expand_for(&name, &header_value, env)?));
    }

    let max_per_minute = match raw.max_per_minute {
        None => DEFAULT_MAX_PER_MINUTE,
        Some(0) => return Err(invalid(&name, "max_per_minute must be at least 1")),
        Some(limit) => limit,
    };

    Ok(Rule {
        name,
        filter: RuleFilter {
            kinds: raw.filter.kinds,
            authors,
            d_prefix: raw.filter.d_prefix,
        },
        webhook: Webhook {
            url,
            method,
            headers,
            body: raw.webhook.body,
        },
        max_per_minute,
    })
}

/// Parse and validate a rules document.
///
/// `env` is the environment used for `${VAR}` expansion — passed explicitly
/// (never read from the process here) so validation is deterministic and
/// testable, matching `buzz-push-gateway`'s config convention.
///
/// # Errors
/// The JSON does not parse, any rule fails validation, a `${VAR}` reference
/// names an unset variable, two rules share a name, or the document lists no
/// rules at all (a bridge with nothing to match is a misconfiguration, not a
/// valid idle state).
pub fn parse_rules(raw: &str, env: &HashMap<String, String>) -> Result<Vec<Rule>, RuleError> {
    let raw_rules: Vec<RawRule> = serde_json::from_str(raw)?;
    if raw_rules.is_empty() {
        return Err(invalid("#0", "the rules document lists no rules"));
    }
    let mut rules = Vec::with_capacity(raw_rules.len());
    let mut seen_names = std::collections::HashSet::new();
    for (index, raw_rule) in raw_rules.into_iter().enumerate() {
        let rule = validate_rule(index, raw_rule, env)?;
        if !seen_names.insert(rule.name.clone()) {
            return Err(invalid(&rule.name, "duplicate rule name"));
        }
        rules.push(rule);
    }
    Ok(rules)
}

// ---------------------------------------------------------------------------
// Event fields, matching, and template expansion
// ---------------------------------------------------------------------------

/// The event fields a template may reference, extracted once per event.
#[derive(Debug, Clone)]
pub struct EventFields {
    /// `{{event.id}}` — hex event id.
    pub id: String,
    /// `{{event.pubkey}}` — hex author pubkey, lowercased.
    pub pubkey: String,
    /// `{{event.kind}}`.
    pub kind: u32,
    /// `{{event.created_at}}` — unix seconds.
    pub created_at: u64,
    /// `{{event.d_tag}}` — the first `d` tag's value, when the event has
    /// one. Substitutes as the empty string when absent.
    pub d_tag: Option<String>,
    /// `{{event.content}}` — never forwarded unless a template literally
    /// references it.
    pub content: String,
}

impl EventFields {
    /// Extract the referenceable fields from a verified event.
    #[must_use]
    pub fn from_event(event: &nostr::Event) -> Self {
        Self {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex().to_ascii_lowercase(),
            kind: buzz_core::kind::event_kind_u32(event),
            created_at: event.created_at.as_secs(),
            d_tag: d_tag(event),
            content: event.content.clone(),
        }
    }

    fn lookup(&self, token: &str) -> Option<Cow<'_, str>> {
        match token {
            "event.id" => Some(Cow::Borrowed(self.id.as_str())),
            "event.pubkey" => Some(Cow::Borrowed(self.pubkey.as_str())),
            "event.kind" => Some(Cow::Owned(self.kind.to_string())),
            "event.created_at" => Some(Cow::Owned(self.created_at.to_string())),
            "event.d_tag" => Some(Cow::Borrowed(self.d_tag.as_deref().unwrap_or(""))),
            "event.content" => Some(Cow::Borrowed(self.content.as_str())),
            _ => None,
        }
    }

    /// Substitute `{{event.*}}` placeholders into `input`, single-pass.
    ///
    /// Substituted values are **never re-scanned**: a `d` tag or content
    /// string that itself contains `{{event.content}}` stays literal text in
    /// the output rather than becoming a second expansion — which is what
    /// keeps "content is only forwarded where a template explicitly asks for
    /// it" true even against an event crafted to smuggle placeholders.
    /// Unrecognized `{{...}}` sequences pass through unchanged.
    #[must_use]
    pub fn substitute(&self, input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start..];
            let Some(end) = after.find("}}") else {
                out.push_str(after);
                return out;
            };
            match self.lookup(&after[2..end]) {
                Some(value) => {
                    out.push_str(&value);
                    rest = &after[end + 2..];
                }
                None => {
                    out.push_str("{{");
                    rest = &after[2..];
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Substitute placeholders into every **string value** of a JSON body
    /// template, recursively. Object keys, numbers, booleans, and nulls pass
    /// through untouched. The body is built as a [`Value`] and substituted
    /// inside strings — never by formatting raw JSON text — so a value
    /// containing quotes or backslashes serializes safely.
    #[must_use]
    pub fn substitute_body(&self, body: &Value) -> Value {
        match body {
            Value::String(text) => Value::String(self.substitute(text)),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| self.substitute_body(item))
                    .collect(),
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), self.substitute_body(value)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// The first `d` tag's value, when the event carries one.
#[must_use]
pub fn d_tag(event: &nostr::Event) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        let parts = tag.as_slice();
        (parts.first().map(String::as_str) == Some("d"))
            .then(|| parts.get(1).cloned().unwrap_or_default())
    })
}

/// Whether `fields` satisfies `rule`'s filter.
///
/// Kind and author are re-checked even though the REQ filter already asked
/// for them — a buggy or compromised relay can hand any subscription any
/// event, and the webhook must not fire on a misrouted one (the same
/// reasoning as `buzz-waker`'s presence tap). `d_prefix` is purely
/// client-side: an event with no `d` tag never matches a rule that sets one.
#[must_use]
pub fn rule_matches(rule: &Rule, fields: &EventFields) -> bool {
    if !rule.filter.kinds.contains(&fields.kind) {
        return false;
    }
    if !rule
        .filter
        .authors
        .iter()
        .any(|author| author == &fields.pubkey)
    {
        return false;
    }
    if let Some(prefix) = &rule.filter.d_prefix {
        match &fields.d_tag {
            Some(d_value) => d_value.starts_with(prefix.as_str()),
            None => false,
        }
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn valid_rules_json() -> String {
        json!([{
            "name": "example",
            "filter": {
                "kinds": [30023],
                "authors": ["ab".repeat(32)],
                "d_prefix": "pr-verdict-"
            },
            "webhook": {
                "url": "https://api.example.com/hook",
                "headers": { "Authorization": "Bearer ${HOOK_TOKEN}" },
                "body": { "ref": "main", "id": "{{event.id}}" }
            }
        }])
        .to_string()
    }

    #[test]
    fn a_valid_rule_parses_with_defaults() {
        let rules = parse_rules(&valid_rules_json(), &env(&[("HOOK_TOKEN", "s3cret")]))
            .expect("valid rules parse");
        assert_eq!(rules.len(), 1);
        let rule = &rules[0];
        assert_eq!(rule.name, "example");
        assert_eq!(rule.filter.kinds, vec![30023]);
        assert_eq!(rule.filter.authors, vec!["ab".repeat(32)]);
        assert_eq!(rule.filter.d_prefix.as_deref(), Some("pr-verdict-"));
        assert_eq!(rule.webhook.method, reqwest::Method::POST);
        assert_eq!(rule.max_per_minute, DEFAULT_MAX_PER_MINUTE);
        assert_eq!(rule.webhook.headers.len(), 1);
        assert_eq!(rule.webhook.headers[0].1.reveal(), "Bearer s3cret");
    }

    #[test]
    fn a_missing_env_var_fails_startup_loudly() {
        let error = parse_rules(&valid_rules_json(), &env(&[])).expect_err("must refuse");
        match error {
            RuleError::MissingEnvVar { rule, var } => {
                assert_eq!(rule, "example");
                assert_eq!(var, "HOOK_TOKEN");
            }
            other => panic!("expected MissingEnvVar, got {other:?}"),
        }
    }

    #[test]
    fn env_expansion_applies_to_the_url_too() {
        let raw = json!([{
            "name": "signed-url",
            "filter": { "kinds": [1], "authors": ["cd".repeat(32)] },
            "webhook": { "url": "https://example.com/hook?sig=${SIGNATURE}" }
        }])
        .to_string();
        let rules = parse_rules(&raw, &env(&[("SIGNATURE", "topsecret")])).expect("parses");
        assert_eq!(
            rules[0].webhook.url.reveal(),
            "https://example.com/hook?sig=topsecret"
        );
        assert_eq!(
            rules[0].webhook.url.template(),
            "https://example.com/hook?sig=${SIGNATURE}"
        );
    }

    #[test]
    fn a_bad_hex_author_is_refused() {
        for bad_author in ["not-hex", "abcd", &"gg".repeat(32), &"ab".repeat(33)] {
            let raw = json!([{
                "name": "bad-author",
                "filter": { "kinds": [1], "authors": [bad_author] },
                "webhook": { "url": "https://example.com" }
            }])
            .to_string();
            let error = parse_rules(&raw, &env(&[])).expect_err("must refuse");
            assert!(
                matches!(error, RuleError::Invalid { .. }),
                "author {bad_author:?} must be refused, got {error:?}"
            );
        }
    }

    #[test]
    fn an_uppercase_author_is_normalized_to_lowercase() {
        let raw = json!([{
            "name": "case",
            "filter": { "kinds": [1], "authors": ["AB".repeat(32)] },
            "webhook": { "url": "https://example.com" }
        }])
        .to_string();
        let rules = parse_rules(&raw, &env(&[])).expect("parses");
        assert_eq!(rules[0].filter.authors, vec!["ab".repeat(32)]);
    }

    #[test]
    fn empty_kinds_empty_authors_and_zero_budget_are_refused() {
        let cases = [
            json!([{ "name": "r", "filter": { "kinds": [], "authors": ["ab".repeat(32)] },
                     "webhook": { "url": "https://example.com" } }]),
            json!([{ "name": "r", "filter": { "kinds": [1], "authors": [] },
                     "webhook": { "url": "https://example.com" } }]),
            json!([{ "name": "r", "filter": { "kinds": [1], "authors": ["ab".repeat(32)] },
                     "webhook": { "url": "https://example.com" }, "max_per_minute": 0 }]),
        ];
        for case in cases {
            let error = parse_rules(&case.to_string(), &env(&[])).expect_err("must refuse");
            assert!(matches!(error, RuleError::Invalid { .. }), "got {error:?}");
        }
    }

    #[test]
    fn unknown_fields_a_bad_method_and_duplicate_names_are_refused() {
        let unknown = json!([{
            "name": "r", "surprise": true,
            "filter": { "kinds": [1], "authors": ["ab".repeat(32)] },
            "webhook": { "url": "https://example.com" }
        }]);
        assert!(matches!(
            parse_rules(&unknown.to_string(), &env(&[])),
            Err(RuleError::Json(_))
        ));

        let bad_method = json!([{
            "name": "r",
            "filter": { "kinds": [1], "authors": ["ab".repeat(32)] },
            "webhook": { "url": "https://example.com", "method": "not a method" }
        }]);
        assert!(matches!(
            parse_rules(&bad_method.to_string(), &env(&[])),
            Err(RuleError::Invalid { .. })
        ));

        let duplicate = json!([
            { "name": "r", "filter": { "kinds": [1], "authors": ["ab".repeat(32)] },
              "webhook": { "url": "https://example.com" } },
            { "name": "r", "filter": { "kinds": [2], "authors": ["cd".repeat(32)] },
              "webhook": { "url": "https://example.com/2" } }
        ]);
        assert!(matches!(
            parse_rules(&duplicate.to_string(), &env(&[])),
            Err(RuleError::Invalid { .. })
        ));
    }

    #[test]
    fn an_empty_rules_document_is_refused() {
        assert!(matches!(
            parse_rules("[]", &env(&[])),
            Err(RuleError::Invalid { .. })
        ));
    }

    /// The secret-hygiene contract: neither `Debug` of a whole rule nor
    /// `Display` of any expanded template may contain an expanded `${VAR}`
    /// value. This is what makes "log the rule" safe everywhere.
    #[test]
    fn debug_and_display_never_reveal_expanded_secrets() {
        const SECRET: &str = "hunter2-the-actual-token";
        let raw = json!([{
            "name": "hygiene",
            "filter": { "kinds": [30023], "authors": ["ab".repeat(32)] },
            "webhook": {
                "url": "https://example.com/hook?token=${HOOK_TOKEN}",
                "headers": { "Authorization": "Bearer ${HOOK_TOKEN}" }
            }
        }])
        .to_string();
        let rules = parse_rules(&raw, &env(&[("HOOK_TOKEN", SECRET)])).expect("parses");
        let rule = &rules[0];

        let debug = format!("{rule:?}");
        assert!(!debug.contains(SECRET), "Debug leaked the secret: {debug}");
        assert!(
            debug.contains("${HOOK_TOKEN}"),
            "Debug should show the template instead: {debug}"
        );

        let url_display = rule.webhook.url.to_string();
        let header_display = rule.webhook.headers[0].1.to_string();
        assert!(!url_display.contains(SECRET));
        assert!(!header_display.contains(SECRET));
        assert_eq!(header_display, "Bearer ${HOOK_TOKEN}");

        // The revealed value is still reachable for building the request.
        assert_eq!(
            rule.webhook.headers[0].1.reveal(),
            format!("Bearer {SECRET}")
        );
    }

    #[test]
    fn an_unterminated_reference_is_refused() {
        let raw = json!([{
            "name": "r",
            "filter": { "kinds": [1], "authors": ["ab".repeat(32)] },
            "webhook": { "url": "https://example.com/${OOPS" }
        }])
        .to_string();
        assert!(matches!(
            parse_rules(&raw, &env(&[("OOPS", "x")])),
            Err(RuleError::Invalid { .. })
        ));
    }

    // -- matching ----------------------------------------------------------

    fn fields(kind: u32, pubkey: &str, d_tag: Option<&str>) -> EventFields {
        EventFields {
            id: "ee".repeat(32),
            pubkey: pubkey.to_string(),
            kind,
            created_at: 1_700_000_000,
            d_tag: d_tag.map(str::to_string),
            content: "content".to_string(),
        }
    }

    fn rule(kinds: Vec<u32>, authors: Vec<String>, d_prefix: Option<&str>) -> Rule {
        Rule {
            name: "match-test".to_string(),
            filter: RuleFilter {
                kinds,
                authors,
                d_prefix: d_prefix.map(str::to_string),
            },
            webhook: Webhook {
                url: expand_env("https://example.com", &HashMap::new()).expect("expands"),
                method: reqwest::Method::POST,
                headers: Vec::new(),
                body: None,
            },
            max_per_minute: DEFAULT_MAX_PER_MINUTE,
        }
    }

    #[test]
    fn matching_requires_the_kind() {
        let author = "ab".repeat(32);
        let rule = rule(vec![30023], vec![author.clone()], None);
        assert!(rule_matches(&rule, &fields(30023, &author, None)));
        assert!(!rule_matches(&rule, &fields(1, &author, None)));
    }

    #[test]
    fn matching_requires_a_pinned_author() {
        let author = "ab".repeat(32);
        let other = "cd".repeat(32);
        let rule = rule(vec![1], vec![author.clone()], None);
        assert!(rule_matches(&rule, &fields(1, &author, None)));
        assert!(
            !rule_matches(&rule, &fields(1, &other, None)),
            "an unpinned author must never fire the webhook, whatever the relay sent"
        );
    }

    #[test]
    fn d_prefix_matches_by_prefix_and_only_by_prefix() {
        let author = "ab".repeat(32);
        let rule = rule(
            vec![1],
            vec![author.clone()],
            Some("pr-verdict-yjc801-buzz-"),
        );
        assert!(rule_matches(
            &rule,
            &fields(1, &author, Some("pr-verdict-yjc801-buzz-101"))
        ));
        assert!(!rule_matches(
            &rule,
            &fields(1, &author, Some("pr-verdict-yjc801-velvet-192"))
        ));
        assert!(
            !rule_matches(
                &rule,
                &fields(1, &author, Some("x-pr-verdict-yjc801-buzz-101"))
            ),
            "a prefix is anchored at the start, not a substring match"
        );
    }

    #[test]
    fn a_missing_d_tag_never_matches_a_d_prefix_rule() {
        let author = "ab".repeat(32);
        let with_prefix = rule(vec![1], vec![author.clone()], Some("pr-"));
        let without_prefix = rule(vec![1], vec![author.clone()], None);
        assert!(!rule_matches(&with_prefix, &fields(1, &author, None)));
        assert!(rule_matches(&without_prefix, &fields(1, &author, None)));
    }

    // -- template expansion ------------------------------------------------

    #[test]
    fn every_placeholder_substitutes() {
        let fields = EventFields {
            id: "aa".repeat(32),
            pubkey: "bb".repeat(32),
            kind: 30023,
            created_at: 1_725_000_000,
            d_tag: Some("pr-verdict-yjc801-buzz-101".to_string()),
            content: "APPROVE".to_string(),
        };
        let input = "{{event.id}}|{{event.pubkey}}|{{event.kind}}|{{event.created_at}}|\
                     {{event.d_tag}}|{{event.content}}";
        assert_eq!(
            fields.substitute(input),
            format!(
                "{}|{}|30023|1725000000|pr-verdict-yjc801-buzz-101|APPROVE",
                "aa".repeat(32),
                "bb".repeat(32)
            )
        );
    }

    #[test]
    fn content_is_only_forwarded_where_explicitly_referenced() {
        let mut fields = fields(1, &"ab".repeat(32), Some("d-value"));
        fields.content = "SECRET CONTENT".to_string();
        let without_content = fields.substitute("id={{event.id}} d={{event.d_tag}}");
        assert!(!without_content.contains("SECRET CONTENT"));
        let with_content = fields.substitute("c={{event.content}}");
        assert_eq!(with_content, "c=SECRET CONTENT");
    }

    #[test]
    fn substituted_values_are_never_rescanned() {
        // An event crafted so its d tag smuggles a content placeholder: the
        // placeholder must land as literal text, not as a second expansion.
        let mut fields = fields(1, &"ab".repeat(32), Some("{{event.content}}"));
        fields.content = "MUST NOT LEAK".to_string();
        let out = fields.substitute("d={{event.d_tag}}");
        assert_eq!(out, "d={{event.content}}");
        assert!(!out.contains("MUST NOT LEAK"));
    }

    #[test]
    fn a_missing_d_tag_substitutes_as_empty_and_unknown_tokens_pass_through() {
        let fields = fields(1, &"ab".repeat(32), None);
        assert_eq!(fields.substitute("[{{event.d_tag}}]"), "[]");
        assert_eq!(
            fields.substitute("{{event.nope}} {{unclosed"),
            "{{event.nope}} {{unclosed"
        );
    }

    #[test]
    fn body_substitution_reaches_nested_strings_and_nothing_else() {
        let fields = fields(30023, &"ab".repeat(32), Some("pr-verdict-101"));
        let body = json!({
            "ref": "main",
            "inputs": {
                "dry_run": "false",
                "coordinate": "{{event.d_tag}}",
                "nested": ["{{event.id}}", 7, true, null]
            },
            "kind": 30023
        });
        let substituted = fields.substitute_body(&body);
        assert_eq!(substituted["inputs"]["coordinate"], "pr-verdict-101");
        assert_eq!(substituted["inputs"]["nested"][0], "ee".repeat(32));
        assert_eq!(substituted["inputs"]["nested"][1], 7);
        assert_eq!(substituted["inputs"]["nested"][2], true);
        assert_eq!(substituted["inputs"]["nested"][3], Value::Null);
        assert_eq!(
            substituted["kind"], 30023,
            "non-strings pass through untouched"
        );
        assert_eq!(substituted["ref"], "main");
    }

    #[test]
    fn body_substitution_serializes_safely_with_quotes_in_values() {
        // The reason the body is built as a Value and substituted inside
        // strings: a value containing quotes must serialize as valid JSON,
        // never break out of its string literal.
        let mut fields = fields(1, &"ab".repeat(32), Some("d\"quoted\\slash"));
        fields.content = "line1\nline2 \"quoted\"".to_string();
        let body = json!({ "d": "{{event.d_tag}}", "c": "{{event.content}}" });
        let substituted = fields.substitute_body(&body);
        let serialized = serde_json::to_string(&substituted).expect("serializes");
        let round_trip: Value = serde_json::from_str(&serialized).expect("valid JSON");
        assert_eq!(round_trip["d"], "d\"quoted\\slash");
        assert_eq!(round_trip["c"], "line1\nline2 \"quoted\"");
    }

    #[test]
    fn d_tag_reads_the_first_d_tag() {
        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(30023), "content")
            .tags([
                nostr::Tag::parse(vec!["t", "unrelated"]).expect("parses"),
                nostr::Tag::parse(vec!["d", "pr-verdict-101"]).expect("parses"),
            ])
            .sign_with_keys(&keys)
            .expect("signs");
        assert_eq!(d_tag(&event).as_deref(), Some("pr-verdict-101"));

        let bare = nostr::EventBuilder::text_note("hi")
            .sign_with_keys(&keys)
            .expect("signs");
        assert_eq!(d_tag(&bare), None);
    }
}
