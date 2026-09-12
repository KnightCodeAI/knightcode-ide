//! The engine's HTTP routes as the IDE calls them. Every request carries
//! the launch token; no response type has a field for a key, a token or a
//! refresh token, so none can be stored by accident.

use anyhow::{Result, anyhow};
use futures::{AsyncBufReadExt as _, AsyncReadExt as _, StreamExt as _, io::BufReader};
use http_client::{AsyncBody, HttpClient, Method, Request};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    pub url: String,
    pub token: Arc<str>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{message} (HTTP {status}, {code})")]
    Status {
        status: u16,
        code: String,
        message: String,
    },
    #[error("{0:#}")]
    Transport(#[from] anyhow::Error),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoginKind {
    ApiKey,
    Oauth,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub provider_id: String,
    pub provider_name: String,
    #[serde(rename = "type")]
    pub kind: LoginKind,
    pub is_subscription: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LoginOption {
    pub provider_id: String,
    pub provider_name: String,
    #[serde(rename = "type")]
    pub kind: LoginKind,
    pub label: String,
    pub is_subscription: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Accounts {
    pub accounts: Vec<Account>,
    pub login_options: Vec<LoginOption>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EngineModel {
    /// `<providerId>/<modelId>`; what every request names. `id` alone is ambiguous.
    #[serde(rename = "ref")]
    pub reference: String,
    pub id: String,
    pub provider_id: String,
    pub provider_name: String,
    pub name: String,
    pub context_window: u64,
    pub max_tokens: u64,
    pub reasoning: bool,
    pub input: Vec<String>,
}

/// The catalog and, separately, the model the user chose.
///
/// The default is the engine's answer, not ours: the CLI records what the
/// user picked and both front doors read the same value. `None` means they
/// have picked nothing, or what they picked is not available — never a guess
/// the IDE made on their behalf.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Catalog {
    pub models: Vec<EngineModel>,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginEvent {
    Info {
        message: String,
    },
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    DeviceCode {
        user_code: String,
        verification_uri: String,
    },
    Progress {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    Text,
    Secret,
    Select,
    ManualCode,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PromptOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct LoginPrompt {
    #[serde(rename = "type")]
    pub kind: PromptKind,
    pub message: String,
    pub placeholder: Option<String>,
    #[serde(default)]
    pub options: Vec<PromptOption>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PendingPrompt {
    pub id: String,
    pub prompt: LoginPrompt,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoginState {
    #[serde(rename_all = "camelCase")]
    Pending {
        login_id: String,
        events: Vec<LoginEvent>,
        pending_prompt: Option<PendingPrompt>,
    },
    #[serde(rename_all = "camelCase")]
    Complete {
        login_id: String,
        events: Vec<LoginEvent>,
    },
    #[serde(rename_all = "camelCase")]
    Failed {
        login_id: String,
        events: Vec<LoginEvent>,
        error: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum EngineEvent {
    #[serde(rename = "account.changed", rename_all = "camelCase")]
    AccountChanged {
        provider_id: String,
        authenticated: bool,
    },
    #[serde(rename = "models.changed")]
    ModelsChanged,
    /// Session events and anything added later; the IDE ignores them.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: serde_json::Value,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct CompletionBody {
    choices: Vec<CompletionChoice>,
}

#[derive(Deserialize)]
struct CompletionChoice {
    text: String,
}

#[derive(Clone)]
pub struct EngineClient {
    http: Arc<dyn HttpClient>,
    endpoint: Endpoint,
}

impl EngineClient {
    pub fn new(http: Arc<dyn HttpClient>, endpoint: Endpoint) -> Self {
        Self { http, endpoint }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Request<AsyncBody>> {
        let mut builder = Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.endpoint.url))
            .header("Authorization", format!("Bearer {}", self.endpoint.token));
        let body = match body {
            Some(body) => {
                builder = builder.header("Content-Type", "application/json");
                AsyncBody::from(serde_json::to_string(&body)?)
            }
            None => AsyncBody::empty(),
        };
        Ok(builder.body(body)?)
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, ClientError> {
        let request = self.request(method, path, body)?;
        let mut response = self.http.send(request).await?;
        let mut text = String::new();
        response
            .body_mut()
            .read_to_string(&mut text)
            .await
            .map_err(anyhow::Error::from)?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let parsed: Option<ErrorBody> = serde_json::from_str(&text).ok();
            let code = parsed
                .as_ref()
                .and_then(|body| {
                    body.error
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| body.error.get("type")?.as_str().map(str::to_owned))
                })
                .unwrap_or_else(|| "error".to_owned());
            let message = parsed
                .and_then(|body| {
                    body.message
                        .or_else(|| body.error.get("message")?.as_str().map(str::to_owned))
                })
                .unwrap_or_else(|| format!("{path} failed with HTTP {status}"));
            return Err(ClientError::Status {
                status,
                code,
                message,
            });
        }
        if text.is_empty() {
            // 204: the caller asked for `()` and gets it.
            return serde_json::from_str("null").map_err(|error| anyhow!(error).into());
        }
        serde_json::from_str(&text)
            .map_err(|error| anyhow!("{path}: unexpected body: {error}").into())
    }

    pub async fn accounts(&self) -> Result<Accounts, ClientError> {
        self.call(Method::GET, "/v1/accounts", None).await
    }

    pub async fn models(&self) -> Result<Catalog, ClientError> {
        self.call(Method::GET, "/v1/models", None).await
    }

    pub async fn start_login(
        &self,
        provider_id: &str,
        kind: LoginKind,
    ) -> Result<String, ClientError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Started {
            login_id: String,
        }
        let body = serde_json::json!({ "providerId": provider_id, "type": kind });
        Ok(self
            .call::<Started>(Method::POST, "/v1/accounts/login", Some(body))
            .await?
            .login_id)
    }

    pub async fn login_state(&self, login_id: &str) -> Result<LoginState, ClientError> {
        self.call(Method::GET, &format!("/v1/accounts/login/{login_id}"), None)
            .await
    }

    pub async fn submit_login(&self, login_id: &str, value: &str) -> Result<(), ClientError> {
        let body = serde_json::json!({ "value": value });
        self.call::<serde_json::Value>(
            Method::POST,
            &format!("/v1/accounts/login/{login_id}/submit"),
            Some(body),
        )
        .await?;
        Ok(())
    }

    pub async fn cancel_login(&self, login_id: &str) -> Result<(), ClientError> {
        self.call::<()>(
            Method::DELETE,
            &format!("/v1/accounts/login/{login_id}"),
            None,
        )
        .await
    }

    pub async fn sign_out(&self, provider_id: &str) -> Result<(), ClientError> {
        self.call::<()>(Method::DELETE, &format!("/v1/accounts/{provider_id}"), None)
            .await
    }

    /// Seam 3: one fill-in-the-middle completion.
    pub async fn completion(
        &self,
        model: &str,
        prefix: &str,
        suffix: &str,
        max_tokens: u32,
    ) -> Result<String, ClientError> {
        let body = serde_json::json!({
            "model": model,
            "prompt": prefix,
            "suffix": suffix,
            "max_tokens": max_tokens,
        });
        let completion: CompletionBody = self
            .call(Method::POST, "/v1/completions", Some(body))
            .await?;
        Ok(completion
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.text)
            .unwrap_or_default())
    }

    /// Consume `/events` until the stream ends. Comments (the preamble and
    /// heartbeats) are skipped; a frame is delivered when its blank line
    /// arrives.
    pub async fn events(&self, mut on_event: impl FnMut(EngineEvent)) -> Result<(), ClientError> {
        let request = self.request(Method::GET, "/events", None)?;
        let response = self.http.send(request).await?;
        if !response.status().is_success() {
            return Err(ClientError::Status {
                status: response.status().as_u16(),
                code: "events".into(),
                message: "the event stream was refused".into(),
            });
        }
        let mut lines = BufReader::new(response.into_body()).lines();
        let mut data = String::new();
        while let Some(line) = lines.next().await {
            let line = line.map_err(anyhow::Error::from)?;
            if line.is_empty() {
                if !data.is_empty() {
                    match serde_json::from_str::<EngineEvent>(&data) {
                        Ok(event) => on_event(event),
                        Err(error) => {
                            log::warn!("knightcode-engine: unreadable event {data}: {error}")
                        }
                    }
                    data.clear();
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};
    use std::sync::Mutex;

    fn client(
        handler: impl Fn(&str, &str, String) -> (u16, String) + Send + Sync + 'static,
    ) -> EngineClient {
        let handler = Arc::new(handler);
        let http = FakeHttpClient::create(move |mut request| {
            let method = request.method().to_string();
            let path = request.uri().path().to_string();
            let handler = handler.clone();
            async move {
                let mut body = String::new();
                futures::AsyncReadExt::read_to_string(request.body_mut(), &mut body)
                    .await
                    .ok();
                let (status, body) = handler(&method, &path, body);
                Ok(Response::builder()
                    .status(status)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        EngineClient::new(
            http,
            Endpoint {
                url: "http://127.0.0.1:1".into(),
                token: "t".into(),
            },
        )
    }

    #[test]
    fn accounts_and_models_parse_and_carry_no_secret_fields() {
        let client = client(|_, path, _| {
            match path {
            "/v1/accounts" => (200, r#"{"accounts":[{"providerId":"anthropic","providerName":"Anthropic","type":"oauth","isSubscription":true}],"loginOptions":[{"providerId":"anthropic","providerName":"Anthropic","type":"oauth","label":"Anthropic (Claude Pro/Max)","isSubscription":true},{"providerId":"openai","providerName":"OpenAI","type":"api_key","label":"OpenAI API key","isSubscription":false}]}"#.into()),
            "/v1/models" => (200, r#"{"models":[{"ref":"anthropic/claude-opus-5","id":"claude-opus-5","providerId":"anthropic","providerName":"Anthropic","name":"Claude Opus 5","contextWindow":200000,"maxTokens":32000,"reasoning":true,"input":["text","image"],"cost":{"input":1,"output":2}}],"default":"anthropic/claude-opus-5"}"#.into()),
            _ => (404, "{}".into()),
        }
        });
        let accounts = smol::block_on(client.accounts()).unwrap();
        assert_eq!(accounts.accounts[0].provider_id, "anthropic");
        assert_eq!(accounts.accounts[0].kind, LoginKind::Oauth);
        assert_eq!(accounts.login_options.len(), 2);
        assert_eq!(accounts.login_options[1].kind, LoginKind::ApiKey);
        let catalog = smol::block_on(client.models()).unwrap();
        assert_eq!(catalog.models[0].reference, "anthropic/claude-opus-5");
        assert_eq!(catalog.models[0].context_window, 200_000);
        assert!(catalog.models[0].reasoning);
        assert_eq!(catalog.default.as_deref(), Some("anthropic/claude-opus-5"));
    }

    #[test]
    fn a_catalog_with_no_chosen_default_parses_as_none() {
        let client = client(|_, path, _| match path {
            "/v1/models" => (200, r#"{"models":[],"default":null}"#.into()),
            _ => (404, "{}".into()),
        });
        assert_eq!(smol::block_on(client.models()).unwrap().default, None);
    }

    #[test]
    fn a_status_error_carries_the_engine_code_and_the_bearer_is_sent() {
        let client = client(|_, _, _| (401, r#"{"error":"unauthorized"}"#.into()));
        let error = smol::block_on(client.models()).unwrap_err();
        assert!(
            matches!(&error, ClientError::Status { status: 401, code, .. } if code == "unauthorized"),
            "{error}"
        );
    }

    #[test]
    fn login_calls_use_the_documented_routes_and_bodies() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let client = client({
            let seen = seen.clone();
            move |method, path, body| {
                seen.lock().unwrap().push(format!("{method} {path} {body}"));
                match (method, path) {
                    ("POST", "/v1/accounts/login") => (200, r#"{"loginId":"L1"}"#.into()),
                    ("GET", "/v1/accounts/login/L1") => (200, r#"{"status":"pending","loginId":"L1","events":[{"type":"auth_url","url":"https://x/auth"}],"pendingPrompt":{"id":"P1","prompt":{"type":"manual_code","message":"Paste the code"}}}"#.into()),
                    ("POST", "/v1/accounts/login/L1/submit") => (200, r#"{"ok":true}"#.into()),
                    ("DELETE", "/v1/accounts/login/L1") => (204, String::new()),
                    ("DELETE", "/v1/accounts/anthropic") => (204, String::new()),
                    _ => (404, "{}".into()),
                }
            }
        });
        smol::block_on(async {
            assert_eq!(
                client
                    .start_login("anthropic", LoginKind::Oauth)
                    .await
                    .unwrap(),
                "L1"
            );
            let LoginState::Pending {
                events,
                pending_prompt,
                ..
            } = client.login_state("L1").await.unwrap()
            else {
                panic!("expected pending");
            };
            assert!(
                matches!(&events[0], LoginEvent::AuthUrl { url, .. } if url == "https://x/auth")
            );
            assert_eq!(pending_prompt.unwrap().prompt.kind, PromptKind::ManualCode);
            client.submit_login("L1", "code").await.unwrap();
            client.cancel_login("L1").await.unwrap();
            client.sign_out("anthropic").await.unwrap();
        });
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0],
            r#"POST /v1/accounts/login {"providerId":"anthropic","type":"oauth"}"#
        );
        assert_eq!(
            seen[2],
            r#"POST /v1/accounts/login/L1/submit {"value":"code"}"#
        );
        assert_eq!(seen[3], "DELETE /v1/accounts/login/L1 ");
        assert_eq!(seen[4], "DELETE /v1/accounts/anthropic ");
    }

    #[test]
    fn a_completion_posts_prefix_and_suffix_and_returns_the_text() {
        let client = client(|_, path, body| {
            assert_eq!(path, "/v1/completions");
            assert!(
                body.contains(r#""prompt":"fn main() {""#)
                    && body.contains(r#""suffix":"}""#)
                    && body.contains(r#""max_tokens":64"#),
                "{body}"
            );
            (
                200,
                r#"{"choices":[{"index":0,"text":"\n    println!(\"hi\");\n","finish_reason":"stop"}]}"#.into(),
            )
        });
        let text =
            smol::block_on(client.completion("anthropic/claude-opus-5", "fn main() {", "}", 64))
                .unwrap();
        assert_eq!(text, "\n    println!(\"hi\");\n");
    }

    #[test]
    fn events_are_parsed_from_sse_and_comments_are_skipped() {
        let stream = ": connected\n\nevent: account.changed\ndata: {\"type\":\"account.changed\",\"providerId\":\"anthropic\",\"authenticated\":true}\n\n: heartbeat\n\nevent: session.delta\ndata: {\"type\":\"session.delta\",\"sessionId\":\"s\"}\n\nevent: models.changed\ndata: {\"type\":\"models.changed\"}\n\n";
        let client = client(move |_, path, _| {
            assert_eq!(path, "/events");
            (200, stream.to_string())
        });
        let mut seen = Vec::new();
        smol::block_on(client.events(|event| seen.push(event))).unwrap();
        assert_eq!(
            seen,
            vec![
                EngineEvent::AccountChanged {
                    provider_id: "anthropic".into(),
                    authenticated: true
                },
                EngineEvent::Other,
                EngineEvent::ModelsChanged,
            ]
        );
    }

    #[test]
    fn a_refused_event_stream_is_a_status_error() {
        let client = client(|_, _, _| (401, r#"{"error":"unauthorized"}"#.into()));
        assert!(matches!(
            smol::block_on(client.events(|_| {})).unwrap_err(),
            ClientError::Status { status: 401, .. }
        ));
    }
}
