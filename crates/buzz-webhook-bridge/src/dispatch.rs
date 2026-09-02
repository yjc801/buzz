//! Dispatch: duplicate suppression, per-rule rate limiting, and the webhook
//! call itself.
//!
//! Everything here is deliberately in-memory and process-local — the crate
//! docs' at-least-once contract means losing this state on restart is
//! acceptable (the resubscribe overlap re-delivers, the [`SeenRing`] is
//! gone, and consumers tolerate duplicates).

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use crate::rules::{EventFields, Rule};

/// How many recently delivered event ids the [`SeenRing`] remembers.
pub const SEEN_RING_CAPACITY: usize = 4096;

/// Per-request timeout for a webhook call, including connect.
pub const WEBHOOK_TIMEOUT_SECS: u64 = 10;

/// An in-memory ring of recently delivered `(rule index, event id)` pairs,
/// used to suppress duplicate deliveries within one process lifetime (a
/// reconnect's 600-second replay overlap re-delivers events the process
/// already handled).
///
/// The key is the pair, not the event id alone: one event can match several
/// rules, and the relay delivers it once per subscription. Keying on the id
/// alone would let whichever subscription arrived first suppress every other
/// rule's webhook for that event — each rule is an independent consumer and
/// deduplicates independently.
///
/// Bounded: when full, the oldest key is evicted — an event replayed after
/// 4096 newer keys slipped past the ring would be re-delivered, which the
/// at-least-once contract permits.
pub struct SeenRing {
    order: VecDeque<(usize, String)>,
    members: HashSet<(usize, String)>,
    capacity: usize,
}

impl SeenRing {
    /// An empty ring holding up to `capacity` keys.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            members: HashSet::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    /// Record `event_id` against the rule at `rule_index`. Returns `true` the
    /// first time that pair is seen, `false` for a duplicate still inside the
    /// ring. Two different rules seeing the same event are two first
    /// sightings.
    pub fn first_sighting(&mut self, rule_index: usize, event_id: &str) -> bool {
        let key = (rule_index, event_id.to_string());
        if self.members.contains(&key) {
            return false;
        }
        if self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.members.remove(&evicted);
            }
        }
        self.order.push_back(key.clone());
        self.members.insert(key);
        true
    }
}

/// A per-rule token bucket: `max_per_minute` calls per minute, refilled
/// continuously, holding at most one minute's budget.
///
/// Time is passed in (`now_ms`) rather than read from a clock, so behavior
/// is deterministic under test. The caller owns the clock — see
/// [`crate::bridge`].
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_per_ms: f64,
    last_refill_ms: u64,
}

impl TokenBucket {
    /// A full bucket allowing `max_per_minute` calls per minute.
    #[must_use]
    pub fn new(max_per_minute: u32, now_ms: u64) -> Self {
        let capacity = f64::from(max_per_minute.max(1));
        Self {
            capacity,
            tokens: capacity,
            refill_per_ms: capacity / 60_000.0,
            last_refill_ms: now_ms,
        }
    }

    /// Take one token if the budget allows it. `false` means the caller
    /// must drop (and log) the dispatch — the bucket never queues.
    pub fn try_take(&mut self, now_ms: u64) -> bool {
        let elapsed_ms = now_ms.saturating_sub(self.last_refill_ms);
        self.last_refill_ms = now_ms;
        self.tokens = (self.tokens + elapsed_ms as f64 * self.refill_per_ms).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// A webhook request fully prepared for sending: env expansion happened at
/// startup, event placeholders are substituted here. The URL may carry
/// secrets — this struct intentionally implements neither `Debug` nor
/// `Display`.
pub struct PreparedRequest {
    method: reqwest::Method,
    url: String,
    headers: Vec<(reqwest::header::HeaderName, String)>,
    body: Option<serde_json::Value>,
}

impl PreparedRequest {
    /// Substitute `fields` into `rule`'s webhook template.
    #[must_use]
    pub fn build(rule: &Rule, fields: &EventFields) -> Self {
        Self {
            method: rule.webhook.method.clone(),
            url: fields.substitute(rule.webhook.url.reveal()),
            headers: rule
                .webhook
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.reveal().to_string()))
                .collect(),
            body: rule
                .webhook
                .body
                .as_ref()
                .map(|body| fields.substitute_body(body)),
        }
    }
}

/// Whether a response status warrants the one retry: transient server-side
/// failure (5xx). 4xx is the caller's own configuration being wrong —
/// retrying cannot fix it.
#[must_use]
pub fn retry_worthy_status(status: u16) -> bool {
    status >= 500
}

/// The HTTP client every delivery shares — connection pooling, rustls, the
/// per-request timeout, and the redirect policy in one place.
///
/// Redirects are **disabled**. A rule may place a `${VAR}` secret in *any*
/// header value (`X-Hook-Token: ${TOKEN}`) or in the url itself, and reqwest
/// only strips a small standard credential set (`Authorization`, cookies,
/// proxy-authorization, WWW-authenticate) when it follows a cross-origin
/// redirect — a custom header keeps its value all the way to whatever host
/// the `Location` names, including a private or link-local address. Following
/// a redirect would also move the request into a trust domain nobody
/// configured. The repo's other outbound webhook path closes the same
/// boundary for the same reason (`crates/buzz-workflow/src/executor.rs`).
/// A 3xx therefore surfaces to [`deliver`] as a plain non-success status.
///
/// # Errors
/// Client construction fails (TLS backend initialization).
pub fn build_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(WEBHOOK_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("buzz-webhook-bridge/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// A transport failure rendered as a fixed class name, safe to log.
///
/// `reqwest::Error` carries the request url and its `Display` appends
/// `for url (...)`. This crate deliberately allows `${VAR}` secrets in that
/// url (a signed url, a query token), so an ordinary DNS/TLS/connect/timeout
/// failure formatted with `%error` would write the secret into the daemon's
/// logs. Every value this returns is a `&'static str`, so no part of the
/// request can travel with it.
#[must_use]
pub fn transport_error_class(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_builder() {
        "malformed-request"
    } else if error.is_redirect() {
        "redirect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_request() {
        "request"
    } else {
        "other"
    }
}

async fn send_once(
    client: &reqwest::Client,
    request: &PreparedRequest,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut builder = client.request(request.method.clone(), &request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if let Some(body) = &request.body {
        builder = builder.json(body);
    }
    builder.send().await
}

/// Deliver one prepared webhook call: one attempt, plus one retry on
/// transport error or 5xx. The final outcome is logged (rule name, the
/// unexpanded URL template, and a status or a [`transport_error_class`] —
/// never the expanded URL, the expanded headers, or a `reqwest::Error`, all
/// three of which can carry a configured secret) and swallowed: a failed
/// webhook must not affect the bridge's own loop, and the consumer's polling
/// fallback owns everything past the retry.
pub async fn deliver(
    client: reqwest::Client,
    rule_name: String,
    url_template: String,
    request: PreparedRequest,
) {
    for attempt in 1..=2u8 {
        match send_once(&client, &request).await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    tracing::info!(
                        rule = %rule_name,
                        url = %url_template,
                        %status,
                        attempt,
                        "webhook delivered"
                    );
                    return;
                }
                if status.is_redirection() {
                    // Redirects are disabled on purpose (see `build_client`);
                    // say so, rather than leaving an operator to puzzle over a
                    // bare 3xx "delivery failed".
                    tracing::error!(
                        rule = %rule_name,
                        url = %url_template,
                        %status,
                        "webhook answered a redirect; redirects are disabled so \
                         configured secrets cannot follow one — point the rule at \
                         the final url instead"
                    );
                    return;
                }
                if retry_worthy_status(status.as_u16()) && attempt == 1 {
                    tracing::warn!(
                        rule = %rule_name,
                        url = %url_template,
                        %status,
                        "webhook answered 5xx; retrying once"
                    );
                    continue;
                }
                tracing::error!(
                    rule = %rule_name,
                    url = %url_template,
                    %status,
                    attempt,
                    "webhook delivery failed; dropping (the consumer's polling fallback owns this now)"
                );
                return;
            }
            Err(error) => {
                // `%error` would carry the expanded url; only the class is
                // safe to log. See `transport_error_class`.
                let class = transport_error_class(&error);
                if attempt == 1 {
                    tracing::warn!(
                        rule = %rule_name,
                        url = %url_template,
                        error_class = class,
                        "webhook transport error; retrying once"
                    );
                    continue;
                }
                tracing::error!(
                    rule = %rule_name,
                    url = %url_template,
                    error_class = class,
                    "webhook delivery failed after retry; dropping"
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::parse_rules;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn fields() -> EventFields {
        EventFields {
            id: "aa".repeat(32),
            pubkey: "ab".repeat(32),
            kind: 30023,
            created_at: 1,
            d_tag: Some("pr-1".to_string()),
            content: "CONTENT".to_string(),
        }
    }

    /// A one-shot HTTP stub on loopback: answers the first connection with
    /// `response` and hands back the raw request text it received.
    ///
    /// Deliberately raw `TcpListener` rather than a mock-HTTP dependency —
    /// what is under test is the transport policy of the real client
    /// [`build_client`] builds, so nothing about the wire may be simulated.
    async fn http_stub(response: String) -> (u16, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let read = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]).into_owned();
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            let _ = tx.send(request);
        });
        (port, rx)
    }

    fn rule_hitting(port: u16, headers: serde_json::Value, env: &HashMap<String, String>) -> Rule {
        let raw = json!([{
            "name": "stub",
            "filter": { "kinds": [30023], "authors": ["ab".repeat(32)] },
            "webhook": {
                "url": format!("http://127.0.0.1:{port}/hook"),
                "headers": headers,
            }
        }])
        .to_string();
        parse_rules(&raw, env).expect("parses").remove(0)
    }

    #[tokio::test]
    async fn a_redirect_is_answered_but_never_followed() {
        // The endpoint an attacker would redirect *to*. Nothing may reach it.
        let (collector_port, collector) =
            http_stub("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_string()).await;
        let (endpoint_port, endpoint) = http_stub(format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: \
             http://127.0.0.1:{collector_port}/collect\r\ncontent-length: 0\r\n\r\n"
        ))
        .await;

        let env: HashMap<String, String> =
            [("TOKEN".to_string(), "s3cret-must-not-travel".to_string())]
                .into_iter()
                .collect();
        // A *custom* header: reqwest strips only Authorization/cookie/proxy
        // credentials across a cross-origin redirect, so this is the value
        // that would actually escape if redirects were followed.
        let rule = rule_hitting(endpoint_port, json!({ "X-Hook-Token": "${TOKEN}" }), &env);
        let request = PreparedRequest::build(&rule, &fields());

        let response = send_once(&build_client().expect("client"), &request)
            .await
            .expect("the configured endpoint answers");
        assert_eq!(
            response.status().as_u16(),
            307,
            "the 3xx surfaces to the caller instead of being followed"
        );

        let seen = endpoint
            .await
            .expect("the configured endpoint saw the call");
        assert!(
            seen.contains("s3cret-must-not-travel"),
            "sanity: the secret does reach the endpoint the rule configured"
        );

        // A followed redirect would connect within milliseconds; half a second
        // of silence on loopback means the policy held.
        assert!(
            tokio::time::timeout(Duration::from_millis(500), collector)
                .await
                .is_err(),
            "the redirect target must never be contacted — following it would hand a configured \
             secret to a host nobody configured"
        );
    }

    #[tokio::test]
    async fn a_transport_error_is_logged_as_a_class_never_as_the_url() {
        // Bind then drop, so the port is closed but known-unused.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);

        let secret = "SUPERSECRET-QUERY-TOKEN";
        let error = build_client()
            .expect("client")
            .post(format!("http://127.0.0.1:{port}/hook?token={secret}"))
            .send()
            .await
            .expect_err("a closed port refuses the connection");

        assert!(
            error.to_string().contains(secret),
            "reqwest's own Display carries the request url — this is precisely the leak \
             transport_error_class exists to stop. If this assertion ever fails, reqwest changed \
             and the sanitizer's rationale needs re-checking (got: {error})"
        );
        assert_eq!(
            transport_error_class(&error),
            "connect",
            "a refused connection classifies as connect"
        );
    }

    #[test]
    fn the_ring_suppresses_duplicates_until_eviction() {
        let mut ring = SeenRing::new(3);
        assert!(ring.first_sighting(0, "a"));
        assert!(!ring.first_sighting(0, "a"), "a duplicate is suppressed");
        assert!(ring.first_sighting(0, "b"));
        assert!(ring.first_sighting(0, "c"));
        // Capacity 3 is full of {a,b,c}; inserting d evicts a.
        assert!(ring.first_sighting(0, "d"));
        assert!(
            ring.first_sighting(0, "a"),
            "an evicted id counts as new again — at-least-once permits the re-delivery"
        );
        assert!(!ring.first_sighting(0, "d"));
    }

    #[test]
    fn one_event_is_a_first_sighting_once_per_rule() {
        let mut ring = SeenRing::new(16);
        assert!(ring.first_sighting(0, "shared"));
        assert!(
            ring.first_sighting(1, "shared"),
            "a second rule matching the same event is its own first sighting — keying on the \
             event id alone would let rule 0 suppress rule 1's webhook"
        );
        assert!(!ring.first_sighting(0, "shared"));
        assert!(!ring.first_sighting(1, "shared"));
    }

    #[test]
    fn a_full_bucket_allows_a_burst_of_exactly_its_budget() {
        let mut bucket = TokenBucket::new(6, 0);
        for call in 0..6 {
            assert!(bucket.try_take(0), "call {call} is within the budget");
        }
        assert!(
            !bucket.try_take(0),
            "the seventh call in the same instant is dropped"
        );
    }

    #[test]
    fn tokens_refill_continuously_and_cap_at_the_budget() {
        let mut bucket = TokenBucket::new(6, 0);
        for _ in 0..6 {
            assert!(bucket.try_take(0));
        }
        assert!(!bucket.try_take(0));
        // 6/min refills one token every 10 seconds.
        assert!(
            !bucket.try_take(9_999),
            "not yet — a token takes 10s at 6/min"
        );
        assert!(bucket.try_take(10_000), "one token has refilled");
        assert!(!bucket.try_take(10_000), "and only one");

        // A long quiet period refills to capacity, never beyond it.
        assert!(bucket.try_take(10_000_000));
        let mut burst = 0;
        while bucket.try_take(10_000_000) {
            burst += 1;
        }
        assert_eq!(
            burst, 5,
            "capacity is one minute's budget, not the whole idle period"
        );
    }

    #[test]
    fn a_custom_max_per_minute_sets_the_budget() {
        let mut bucket = TokenBucket::new(2, 0);
        assert!(bucket.try_take(0));
        assert!(bucket.try_take(0));
        assert!(!bucket.try_take(0));
        // 2/min refills one token every 30 seconds.
        assert!(bucket.try_take(30_000));
    }

    #[test]
    fn time_going_backwards_does_not_mint_tokens() {
        let mut bucket = TokenBucket::new(1, 60_000);
        assert!(bucket.try_take(60_000));
        assert!(
            !bucket.try_take(0),
            "a clock step backwards must not refill"
        );
    }

    #[test]
    fn only_5xx_is_retry_worthy() {
        assert!(retry_worthy_status(500));
        assert!(retry_worthy_status(503));
        assert!(!retry_worthy_status(200));
        assert!(!retry_worthy_status(404));
        assert!(
            !retry_worthy_status(422),
            "a 4xx is our config being wrong; retrying cannot fix it"
        );
    }

    #[test]
    fn a_prepared_request_substitutes_url_and_body_but_not_headers() {
        let raw = json!([{
            "name": "prep",
            "filter": { "kinds": [30023], "authors": ["ab".repeat(32)] },
            "webhook": {
                "url": "https://example.com/hooks/{{event.d_tag}}?token=${TOKEN}",
                "headers": { "X-Static": "{{event.content}}" },
                "body": { "id": "{{event.id}}" }
            }
        }])
        .to_string();
        let env: HashMap<String, String> = [("TOKEN".to_string(), "tok".to_string())]
            .into_iter()
            .collect();
        let rule = &parse_rules(&raw, &env).expect("parses")[0];
        let fields = crate::rules::EventFields {
            id: "aa".repeat(32),
            pubkey: "ab".repeat(32),
            kind: 30023,
            created_at: 1,
            d_tag: Some("pr-1".to_string()),
            content: "CONTENT".to_string(),
        };
        let request = PreparedRequest::build(rule, &fields);
        assert_eq!(request.url, "https://example.com/hooks/pr-1?token=tok");
        assert_eq!(
            request.body.as_ref().expect("a body")["id"],
            "aa".repeat(32)
        );
        assert_eq!(
            request.headers[0].1, "{{event.content}}",
            "event placeholders substitute into the url and body only, not headers"
        );
    }
}
