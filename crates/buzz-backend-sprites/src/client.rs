//! The real [`Substrate`]: Fly Sprites over REST (reqwest) and the exec
//! WebSocket (tokio-tungstenite).
//!
//! Two transport rules are enforced by construction rather than convention:
//!
//! - **The exec URL builder has no env parameter.** The Sprites exec API
//!   accepts `env=K=V` query parameters, but URLs reach access logs and
//!   proxies; anything secret-bearing travels as WebSocket *data* (stdin
//!   frames) into a tmpfs file instead. A test pins the absence.
//! - **Close-and-drop.** After the exit (or `session_info`) frame, the
//!   client sends a Close frame and drops the TCP stream without awaiting
//!   the server's reciprocal Close — the server otherwise holds the socket
//!   ~5s per exec (documented in the Python SDK, verified against the
//!   platform).

use crate::config::ProviderConfig;
use crate::credentials;
use crate::substrate::{
    CreateOutcome, ExecResult, SessionMeta, SpriteMeta, Substrate, SubstrateError, UrlAuth,
};
use futures_util::{SinkExt, StreamExt};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite;

/// Non-TTY exec stream framing: every binary frame is `[StreamID][payload]`.
mod frame {
    pub const STDIN: u8 = 0;
    pub const STDOUT: u8 = 1;
    pub const STDERR: u8 = 2;
    pub const EXIT: u8 = 3;
    pub const STDIN_EOF: u8 = 4;

    /// What one server frame means to the exec loop.
    #[derive(Debug, PartialEq, Eq)]
    pub enum Incoming {
        Stdout(Vec<u8>),
        Stderr(Vec<u8>),
        Exit(u8),
        /// Anything the loop skips: port notifications, `control:` frames,
        /// unknown stream ids (forward compatibility).
        Ignored,
        /// The first text frame — carries the session id.
        SessionInfo { session_id: String },
    }

    pub fn encode_stdin(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() + 1);
        out.push(STDIN);
        out.extend_from_slice(data);
        out
    }

    pub fn eof() -> Vec<u8> {
        vec![STDIN_EOF]
    }

    pub fn decode_binary(payload: &[u8]) -> Incoming {
        match payload.split_first() {
            Some((&STDOUT, rest)) => Incoming::Stdout(rest.to_vec()),
            Some((&STDERR, rest)) => Incoming::Stderr(rest.to_vec()),
            Some((&EXIT, rest)) => Incoming::Exit(rest.first().copied().unwrap_or(0)),
            _ => Incoming::Ignored,
        }
    }

    pub fn decode_text(text: &str) -> Incoming {
        // Control-connection frames are prefixed with the literal `control:`
        // and are not JSON; skip them.
        if text.starts_with("control:") {
            return Incoming::Ignored;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Incoming::Ignored;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("session_info") => match value.get("session_id").and_then(|s| s.as_str()) {
                Some(id) => Incoming::SessionInfo {
                    session_id: id.to_string(),
                },
                None => Incoming::Ignored,
            },
            // TTY sessions deliver the exit as a JSON text frame.
            Some("exit") => Incoming::Exit(
                value
                    .get("exit_code")
                    .and_then(|c| c.as_u64())
                    .unwrap_or(0) as u8,
            ),
            _ => Incoming::Ignored,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn binary_frames_decode_by_stream_id() {
            assert_eq!(decode_binary(&[STDOUT, b'h', b'i']), Incoming::Stdout(b"hi".to_vec()));
            assert_eq!(decode_binary(&[STDERR, b'e']), Incoming::Stderr(b"e".to_vec()));
            assert_eq!(decode_binary(&[EXIT, 143]), Incoming::Exit(143));
            assert_eq!(decode_binary(&[EXIT]), Incoming::Exit(0));
            assert_eq!(decode_binary(&[9, 1]), Incoming::Ignored);
            assert_eq!(decode_binary(&[]), Incoming::Ignored);
        }

        #[test]
        fn stdin_frames_carry_the_stream_id() {
            assert_eq!(encode_stdin(b"data"), vec![STDIN, b'd', b'a', b't', b'a']);
            assert_eq!(eof(), vec![STDIN_EOF]);
        }

        #[test]
        fn text_frames_parse_session_info_and_exit() {
            assert_eq!(
                decode_text(r#"{"type":"session_info","tty":true,"session_id":"705"}"#),
                Incoming::SessionInfo { session_id: "705".into() }
            );
            assert_eq!(decode_text(r#"{"type":"exit","exit_code":143}"#), Incoming::Exit(143));
            assert_eq!(decode_text(r#"{"type":"port_opened","port":8080}"#), Incoming::Ignored);
            assert_eq!(decode_text("control:{\"type\":\"keepalive\"}"), Incoming::Ignored);
            assert_eq!(decode_text("not json"), Incoming::Ignored);
        }
    }
}

/// Build an exec WebSocket URL. This is the ONLY place exec URLs are built,
/// and it has no env parameter by construction (see module docs).
fn exec_url(
    ws_base: &str,
    sprite: &str,
    argv: &[String],
    dir: Option<&str>,
    tty_detachable: bool,
    stdin: bool,
) -> Result<String, SubstrateError> {
    let mut url = reqwest::Url::parse(&format!("{ws_base}/v1/sprites/{sprite}/exec"))
        .map_err(|e| SubstrateError(format!("could not build the exec URL: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        for (i, arg) in argv.iter().enumerate() {
            q.append_pair("cmd", arg);
            if i == 0 {
                q.append_pair("path", arg);
            }
        }
        if let Some(dir) = dir {
            q.append_pair("dir", dir);
        }
        if tty_detachable {
            q.append_pair("tty", "true");
            q.append_pair("detachable", "true");
        }
        q.append_pair("stdin", if stdin { "true" } else { "false" });
    }
    Ok(url.into())
}

/// The Sprites API client. Also the operation clock: constructed once per
/// deploy, so `elapsed` measures the whole operation.
pub struct SpritesClient {
    http: reqwest::Client,
    /// e.g. `https://api.sprites.dev` (no trailing slash).
    base: String,
    /// The same host with a WebSocket scheme.
    ws_base: String,
    token: String,
    started: Instant,
}

impl SpritesClient {
    /// Resolve ambient credentials and build the client. No network I/O —
    /// the first request happens inside the reconciler, after every refusal
    /// has had its chance.
    pub fn connect(cfg: &ProviderConfig) -> Result<Self, String> {
        let credential = credentials::resolve(cfg.org.as_deref())?;
        let base = std::env::var("SPRITES_API_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| "https://api.sprites.dev".to_string());
        Ok(Self::with_base(base.trim_end_matches('/').to_string(), credential.token))
    }

    /// Used directly by tests to point at a local stub server.
    pub fn with_base(base: String, token: String) -> Self {
        // Idempotent here as well as in main(): test code paths construct the
        // client without going through main, and with both `ring` and
        // `aws-lc-rs` compiled in (feature unification), rustls cannot
        // auto-select a provider.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ws_base = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            format!("wss://{base}")
        };
        Self {
            http: reqwest::Client::new(),
            base,
            ws_base,
            token,
            started: Instant::now(),
        }
    }

    fn rest(&self, path: &str) -> String {
        format!("{}/v1{path}", self.base)
    }

    /// Map a non-success REST response into either a structured outcome the
    /// caller handles or a `SubstrateError` naming status and body.
    async fn error_body(response: reqwest::Response) -> (u16, serde_json::Value) {
        let status = response.status().as_u16();
        let body = response
            .json::<serde_json::Value>()
            .await
            .unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    /// Open the exec WebSocket with the Bearer header.
    async fn dial(
        &self,
        url: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        SubstrateError,
    > {
        use tungstenite::client::IntoClientRequest;
        let mut request = url
            .into_client_request()
            .map_err(|e| SubstrateError(format!("could not build the exec request: {e}")))?;
        request.headers_mut().insert(
            tungstenite::http::header::AUTHORIZATION,
            format!("Bearer {}", self.token)
                .parse()
                .map_err(|_| SubstrateError("could not encode the authorization header".into()))?,
        );
        let (stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| SubstrateError(format!("exec connection failed: {e}")))?;
        Ok(stream)
    }
}

impl Substrate for SpritesClient {
    async fn get_sprite(&self, name: &str) -> Result<Option<SpriteMeta>, SubstrateError> {
        let response = self
            .http
            .get(self.rest(&format!("/sprites/{name}")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SubstrateError(format!("GET sprite failed: {e}")))?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        if !response.status().is_success() {
            let (status, body) = Self::error_body(response).await;
            return Err(SubstrateError(format!("GET sprite returned {status}: {body}")));
        }
        let value = response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| SubstrateError(format!("GET sprite returned malformed JSON: {e}")))?;
        Ok(Some(sprite_meta(&value)))
    }

    async fn create_sprite(
        &self,
        name: &str,
        labels: &[String],
    ) -> Result<CreateOutcome, SubstrateError> {
        let body = serde_json::json!({
            "name": name,
            "labels": labels,
            "url_settings": {"auth": UrlAuth::Sprite.as_str()},
            "wait_for_capacity": true,
        });
        let response = self
            .http
            .post(self.rest("/sprites"))
            .bearer_auth(&self.token)
            // Sprite creation can wait for VM capacity; give it well past the
            // SDKs' 120s create budget but stay inside the deploy deadline.
            .timeout(Duration::from_secs(180))
            .json(&body)
            .send()
            .await
            .map_err(|e| SubstrateError(format!("POST sprite failed: {e}")))?;

        if response.status().is_success() {
            let value = response
                .json::<serde_json::Value>()
                .await
                .map_err(|e| SubstrateError(format!("POST sprite returned malformed JSON: {e}")))?;
            return Ok(CreateOutcome::Created(sprite_meta(&value)));
        }

        let (status, body) = Self::error_body(response).await;
        let code = body.get("error").and_then(|e| e.as_str()).unwrap_or_default();
        let message = body
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        match (status, code) {
            // 409 is the documented duplicate-name answer; the legacy API
            // also reported it as a 400 whose message names the collision.
            (409, _) => Ok(CreateOutcome::AlreadyExists),
            (400, _) if message.to_ascii_lowercase().contains("already exists") => {
                Ok(CreateOutcome::AlreadyExists)
            }
            (_, "sprite_creation_rate_limited") | (429, _) => {
                let retry_after = body
                    .get("retry_after_seconds")
                    .and_then(|s| s.as_u64())
                    .unwrap_or(5);
                Ok(CreateOutcome::CreationRateLimited {
                    retry_after: Duration::from_secs(retry_after),
                })
            }
            (_, "concurrent_sprite_limit_exceeded") => Ok(CreateOutcome::ConcurrentLimit {
                message: if message.is_empty() {
                    "the organization's concurrent-sprite limit is reached".to_string()
                } else {
                    message
                },
            }),
            _ => Err(SubstrateError(format!("POST sprite returned {status}: {body}"))),
        }
    }

    async fn set_url_settings(&self, name: &str, auth: UrlAuth) -> Result<(), SubstrateError> {
        let response = self
            .http
            .put(self.rest(&format!("/sprites/{name}")))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({"url_settings": {"auth": auth.as_str()}}))
            .send()
            .await
            .map_err(|e| SubstrateError(format!("PUT url_settings failed: {e}")))?;
        if !response.status().is_success() {
            let (status, body) = Self::error_body(response).await;
            return Err(SubstrateError(format!("PUT url_settings returned {status}: {body}")));
        }
        Ok(())
    }

    async fn list_sessions(&self, name: &str) -> Result<Vec<SessionMeta>, SubstrateError> {
        let response = self
            .http
            .get(self.rest(&format!("/sprites/{name}/exec")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SubstrateError(format!("GET sessions failed: {e}")))?;
        if !response.status().is_success() {
            let (status, body) = Self::error_body(response).await;
            return Err(SubstrateError(format!("GET sessions returned {status}: {body}")));
        }
        let value = response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| SubstrateError(format!("GET sessions returned malformed JSON: {e}")))?;
        Ok(value
            .get("sessions")
            .and_then(|s| s.as_array())
            .map(|sessions| {
                sessions
                    .iter()
                    .map(|s| SessionMeta {
                        id: s.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        command: s
                            .get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        tty: s.get("tty").and_then(|v| v.as_bool()).unwrap_or(false),
                        is_active: s.get("is_active").and_then(|v| v.as_bool()).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn run(
        &self,
        name: &str,
        argv: &[String],
        stdin: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<ExecResult, SubstrateError> {
        let url = exec_url(&self.ws_base, name, argv, None, false, stdin.is_some())?;
        let work = async {
            let mut stream = self.dial(&url).await?;

            // Stdin first (the server expects it when announced), then the
            // mandatory EOF frame — without it the process waits forever.
            if let Some(data) = stdin {
                stream
                    .send(tungstenite::Message::Binary(frame::encode_stdin(&data).into()))
                    .await
                    .map_err(|e| SubstrateError(format!("could not stream stdin: {e}")))?;
            }
            stream
                .send(tungstenite::Message::Binary(frame::eof().into()))
                .await
                .map_err(|e| SubstrateError(format!("could not close stdin: {e}")))?;

            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut ping = tokio::time::interval(Duration::from_secs(15));
            ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ping.tick() => {
                        let _ = stream.send(tungstenite::Message::Ping(Vec::new().into())).await;
                    }
                    message = stream.next() => {
                        let Some(message) = message else {
                            return Err(SubstrateError(
                                "exec connection closed before the process exited".into(),
                            ));
                        };
                        let message = message
                            .map_err(|e| SubstrateError(format!("exec stream failed: {e}")))?;
                        let incoming = match &message {
                            tungstenite::Message::Binary(b) => frame::decode_binary(b),
                            tungstenite::Message::Text(t) => frame::decode_text(t.as_str()),
                            _ => frame::Incoming::Ignored,
                        };
                        match incoming {
                            frame::Incoming::Stdout(mut b) => stdout.append(&mut b),
                            frame::Incoming::Stderr(mut b) => stderr.append(&mut b),
                            frame::Incoming::Exit(code) => {
                                // Close-and-drop: do not await the server's
                                // reciprocal Close (see module docs).
                                let _ = stream.send(tungstenite::Message::Close(None)).await;
                                drop(stream);
                                return Ok(ExecResult {
                                    exit_code: i32::from(code),
                                    stdout: String::from_utf8_lossy(&stdout).into_owned(),
                                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                                });
                            }
                            frame::Incoming::SessionInfo { .. } | frame::Incoming::Ignored => {}
                        }
                    }
                }
            }
        };
        tokio::time::timeout(timeout, work)
            .await
            .map_err(|_| SubstrateError(format!("exec did not finish within {timeout:?}")))?
    }

    async fn start_detached(
        &self,
        name: &str,
        argv: &[String],
        dir: &str,
    ) -> Result<String, SubstrateError> {
        let url = exec_url(&self.ws_base, name, argv, Some(dir), true, false)?;
        let work = async {
            let mut stream = self.dial(&url).await?;
            loop {
                let Some(message) = stream.next().await else {
                    return Err(SubstrateError(
                        "detached spawn closed before session_info arrived".into(),
                    ));
                };
                let message =
                    message.map_err(|e| SubstrateError(format!("detached spawn failed: {e}")))?;
                let incoming = match &message {
                    tungstenite::Message::Text(t) => frame::decode_text(t.as_str()),
                    tungstenite::Message::Binary(b) => frame::decode_binary(b),
                    _ => frame::Incoming::Ignored,
                };
                match incoming {
                    frame::Incoming::SessionInfo { session_id } => {
                        // The session lives server-side (tmux); hang up
                        // without waiting (close-and-drop).
                        let _ = stream.send(tungstenite::Message::Close(None)).await;
                        drop(stream);
                        return Ok(session_id);
                    }
                    frame::Incoming::Exit(code) => {
                        return Err(SubstrateError(format!(
                            "the launcher exited (code {code}) before the session was established"
                        )));
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), work)
            .await
            .map_err(|_| SubstrateError("detached spawn timed out waiting for session_info".into()))?
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

fn sprite_meta(value: &serde_json::Value) -> SpriteMeta {
    SpriteMeta {
        name: value.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        status: value
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        labels: value
            .get("labels")
            .and_then(|l| l.as_array())
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|l| l.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        url_auth: value
            .get("url_settings")
            .and_then(|s| s.get("auth"))
            .and_then(|a| a.as_str())
            .map(str::to_string),
    }
}

#[cfg(test)]
impl SpritesClient {
    /// Test-only cleanup. Deliberately NOT on the `Substrate` trait — the
    /// reconciler must remain unable to express a delete.
    async fn delete_sprite(&self, name: &str) -> Result<(), SubstrateError> {
        let response = self
            .http
            .delete(self.rest(&format!("/sprites/{name}")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SubstrateError(format!("DELETE sprite failed: {e}")))?;
        if !response.status().is_success() && response.status().as_u16() != 404 {
            return Err(SubstrateError(format!(
                "DELETE sprite returned {}",
                response.status()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE transport rule: no exec URL ever carries env values. The builder
    /// is the only URL source, so pinning it here pins the client.
    #[test]
    fn exec_urls_never_carry_env() {
        let argv = vec!["bash".to_string(), "-c".to_string(), "env SECRET=x".to_string()];
        let url = exec_url("wss://api.sprites.dev", "s", &argv, Some("/home/sprite"), true, false)
            .unwrap();
        assert!(!url.contains("env="), "env leaked into the URL: {url}");
        assert!(url.contains("tty=true") && url.contains("detachable=true"));
        assert!(url.contains("stdin=false"));
        assert!(url.contains("dir=%2Fhome%2Fsprite"));
    }

    #[test]
    fn exec_url_repeats_cmd_and_sets_path_to_argv0() {
        let argv = vec!["sh".to_string(), "-c".to_string(), "echo hi".to_string()];
        let url = exec_url("wss://h", "spr", &argv, None, false, true).unwrap();
        assert!(url.starts_with("wss://h/v1/sprites/spr/exec?"));
        let query: Vec<(String, String)> = reqwest::Url::parse(&url)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let cmds: Vec<&str> = query
            .iter()
            .filter(|(k, _)| k == "cmd")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(cmds, ["sh", "-c", "echo hi"]);
        assert!(query.contains(&("path".to_string(), "sh".to_string())));
        assert!(query.contains(&("stdin".to_string(), "true".to_string())));
    }

    #[test]
    fn sprite_meta_parses_the_live_shape() {
        // Field shapes from a live GET (2026-08-06).
        let value = serde_json::json!({
            "id": "sprite-41b191c2",
            "name": "buzz-spike-1",
            "status": "warm",
            "labels": ["buzz.block.xyz/managed-by=buzz-backend-sprites"],
            "url_settings": {"auth": "sprite", "private_access": "admins"},
        });
        let meta = sprite_meta(&value);
        assert_eq!(meta.name, "buzz-spike-1");
        assert_eq!(meta.status, "warm");
        assert_eq!(meta.labels.len(), 1);
        assert_eq!(meta.url_auth.as_deref(), Some("sprite"));
    }

    #[test]
    fn ws_base_derives_from_http_base() {
        let c = SpritesClient::with_base("https://api.sprites.dev".into(), "t".into());
        assert_eq!(c.ws_base, "wss://api.sprites.dev");
        let c = SpritesClient::with_base("http://127.0.0.1:8080".into(), "t".into());
        assert_eq!(c.ws_base, "ws://127.0.0.1:8080");
    }

    /// The whole client surface against the real platform, gated on
    /// `BUZZ_SPRITES_LIVE=1` + an ambient token. Creates one throwaway
    /// sprite and deletes it; costs cents.
    ///
    /// Run: `BUZZ_SPRITES_LIVE=1 SPRITE_TOKEN=… cargo test -p
    /// buzz-backend-sprites live_client_round_trip -- --nocapture`
    #[test]
    fn live_client_round_trip() {
        if std::env::var("BUZZ_SPRITES_LIVE").as_deref() != Ok("1") {
            eprintln!("live_client_round_trip: skipped (set BUZZ_SPRITES_LIVE=1)");
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let cfg = crate::config::parse(&serde_json::Value::Null).unwrap();
            let client = SpritesClient::connect(&cfg).expect("no ambient credential");
            let name = format!("buzz-live-{}", std::process::id());
            let labels = vec![
                "buzz.block.xyz/managed-by=buzz-backend-sprites".to_string(),
                format!("buzz.block.xyz/agent-pubkey-full={}", "a".repeat(64)),
            ];

            // Absent → create → labels round-trip on a fresh GET.
            assert_eq!(client.get_sprite(&name).await.unwrap(), None);
            let created = client.create_sprite(&name, &labels).await.unwrap();
            let CreateOutcome::Created(meta) = created else {
                panic!("expected Created, got {created:?}");
            };
            assert_eq!(meta.labels, labels, "labels did not round-trip on create");
            let fetched = client.get_sprite(&name).await.unwrap().expect("gone after create");
            assert_eq!(fetched.labels, labels, "labels did not round-trip on GET");
            assert_eq!(fetched.url_auth.as_deref(), Some("sprite"));

            // Idempotent create: second POST reports the taken name.
            let raced = client.create_sprite(&name, &labels).await.unwrap();
            assert_eq!(raced, CreateOutcome::AlreadyExists, "duplicate create");

            // Non-TTY exec with stdin streamed as data frames.
            let result = client
                .run(
                    &name,
                    &["sh".into(), "-c".into(), "cat; echo tail; echo err >&2".into()],
                    Some(b"stdin-bytes\n".to_vec()),
                    Duration::from_secs(60),
                )
                .await
                .unwrap();
            assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
            assert_eq!(result.stdout, "stdin-bytes\ntail\n");
            assert_eq!(result.stderr, "err\n");

            // Exit codes survive the frame protocol.
            let failing = client
                .run(&name, &["sh".into(), "-c".into(), "exit 7".into()], None, Duration::from_secs(30))
                .await
                .unwrap();
            assert_eq!(failing.exit_code, 7);

            // Detached TTY spawn: session_info arrives, the session survives
            // our disconnect, and the session list shows it.
            let session = client
                .start_detached(
                    &name,
                    &["bash".into(), "-c".into(), "sleep 120".into()],
                    "/home/sprite",
                )
                .await
                .unwrap();
            assert!(!session.is_empty());
            let sessions = client.list_sessions(&name).await.unwrap();
            assert!(
                sessions.iter().any(|s| s.id == session),
                "detached session {session} missing from {sessions:?}"
            );

            client.delete_sprite(&name).await.unwrap();
            eprintln!("live_client_round_trip: PASS ({name} created and destroyed)");
        });
    }
}
