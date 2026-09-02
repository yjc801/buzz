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

/// An in-memory ring of recently seen event ids, used to suppress duplicate
/// deliveries within one process lifetime (a reconnect's 600-second replay
/// overlap re-delivers events the process already handled).
///
/// Bounded: when full, the oldest id is evicted — an event replayed after
/// 4096 newer ones slipped past the ring would be re-delivered, which the
/// at-least-once contract permits.
pub struct SeenRing {
    order: VecDeque<String>,
    members: HashSet<String>,
    capacity: usize,
}

impl SeenRing {
    /// An empty ring holding up to `capacity` ids.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            members: HashSet::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    /// Record `event_id`. Returns `true` the first time an id is seen,
    /// `false` for a duplicate still inside the ring.
    pub fn first_sighting(&mut self, event_id: &str) -> bool {
        if self.members.contains(event_id) {
            return false;
        }
        if self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.members.remove(&evicted);
            }
        }
        self.order.push_back(event_id.to_string());
        self.members.insert(event_id.to_string());
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
    headers: Vec<(String, String)>,
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

/// The HTTP client every delivery shares — connection pooling, rustls, and
/// the per-request timeout in one place.
///
/// # Errors
/// Client construction fails (TLS backend initialization).
pub fn build_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(WEBHOOK_TIMEOUT_SECS))
        .user_agent(concat!("buzz-webhook-bridge/", env!("CARGO_PKG_VERSION")))
        .build()
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
/// transport error or 5xx. The final outcome is logged (rule name and the
/// unexpanded URL template only — never the expanded URL or headers) and
/// swallowed: a failed webhook must not affect the bridge's own loop, and
/// the consumer's polling fallback owns everything past the retry.
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
                if attempt == 1 {
                    tracing::warn!(
                        rule = %rule_name,
                        url = %url_template,
                        %error,
                        "webhook transport error; retrying once"
                    );
                    continue;
                }
                tracing::error!(
                    rule = %rule_name,
                    url = %url_template,
                    %error,
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

    #[test]
    fn the_ring_suppresses_duplicates_until_eviction() {
        let mut ring = SeenRing::new(3);
        assert!(ring.first_sighting("a"));
        assert!(!ring.first_sighting("a"), "a duplicate is suppressed");
        assert!(ring.first_sighting("b"));
        assert!(ring.first_sighting("c"));
        // Capacity 3 is full of {a,b,c}; inserting d evicts a.
        assert!(ring.first_sighting("d"));
        assert!(
            ring.first_sighting("a"),
            "an evicted id counts as new again — at-least-once permits the re-delivery"
        );
        assert!(!ring.first_sighting("d"));
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
