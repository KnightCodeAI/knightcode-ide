//! One login, driven from the IDE: start it on the engine, poll its state,
//! report each event once (a URL to open, a device code to show) and each
//! prompt once, and end complete or failed. The engine holds
//! the OAuth exchange and writes the credential; this side only relays.

use crate::client::{ClientError, EngineClient, LoginEvent, LoginKind, LoginState, PendingPrompt};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Client(#[from] ClientError),
}

#[derive(Debug)]
pub enum LoginOutcome {
    Complete,
    /// The engine needs a value this surface cannot ask for; the login is
    /// still pending, so `cancel` it or hand it to one that can prompt.
    Prompt(PendingPrompt),
}

pub struct Login {
    client: EngineClient,
    pub id: String,
    reported: usize,
    /// The prompt already handed to `on_prompt`, so a poll that still sees it
    /// does not offer it twice.
    reported_prompt: Option<String>,
    pub poll_interval: Duration,
}

impl Login {
    pub async fn start(
        client: EngineClient,
        provider_id: &str,
        kind: LoginKind,
    ) -> Result<Self, ClientError> {
        let id = client.start_login(provider_id, kind).await?;
        Ok(Self {
            client,
            id,
            reported: 0,
            reported_prompt: None,
            poll_interval: Duration::from_millis(500),
        })
    }

    /// Poll until the login settles. `on_event` sees each engine event once;
    /// `on_prompt` sees each prompt once and answers whether to keep polling.
    ///
    /// A prompt is not the end of a login: an OAuth flow offers the paste
    /// fallback *while* its loopback callback is still in flight, so the
    /// engine can settle on its own with the box on screen. A caller that can
    /// show the box says `true` here and lets whichever side wins finish the
    /// login; one that cannot says `false` and gets the prompt back.
    pub async fn advance(
        &mut self,
        mut on_event: impl FnMut(&LoginEvent),
        mut on_prompt: impl FnMut(&PendingPrompt) -> bool,
    ) -> Result<LoginOutcome, LoginError> {
        loop {
            let state = self.client.login_state(&self.id).await?;
            let (events, pending_prompt, complete) = match state {
                LoginState::Pending {
                    events,
                    pending_prompt,
                    ..
                } => (events, pending_prompt, false),
                LoginState::Complete { events, .. } => (events, None, true),
                LoginState::Failed { error, .. } => return Err(LoginError::Failed(error)),
            };
            for event in events.iter().skip(self.reported) {
                on_event(event);
            }
            self.reported = self.reported.max(events.len());
            if complete {
                return Ok(LoginOutcome::Complete);
            }
            if let Some(prompt) = pending_prompt
                && self.reported_prompt.as_deref() != Some(prompt.id.as_str())
            {
                self.reported_prompt = Some(prompt.id.clone());
                if !on_prompt(&prompt) {
                    return Ok(LoginOutcome::Prompt(prompt));
                }
            }
            // A real timer: this module runs outside gpui so it can be
            // driven from the panel's connection and tested under smol.
            #[allow(clippy::disallowed_methods)]
            let poll = smol::Timer::after(self.poll_interval);
            poll.await;
        }
    }

    pub async fn submit(&self, value: &str) -> Result<(), ClientError> {
        self.client.submit_login(&self.id, value).await
    }

    pub async fn cancel(&self) -> Result<(), ClientError> {
        self.client.cancel_login(&self.id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Endpoint, EngineClient};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    /// A login that is pending with an auth_url for the first two polls,
    /// then complete.
    fn client_for(states: &'static [&'static str]) -> EngineClient {
        let polls = Arc::new(AtomicUsize::new(0));
        let http = FakeHttpClient::create(move |request| {
            let polls = polls.clone();
            let path = request.uri().path().to_string();
            async move {
                let body = match path.as_str() {
                    "/v1/accounts/login" => r#"{"loginId":"L1"}"#.to_string(),
                    "/v1/accounts/login/L1" => {
                        let index = polls.fetch_add(1, Ordering::SeqCst).min(states.len() - 1);
                        states[index].to_string()
                    }
                    "/v1/accounts/login/L1/submit" => r#"{"ok":true}"#.to_string(),
                    _ => "{}".to_string(),
                };
                Ok(Response::builder()
                    .status(200)
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

    const PENDING_URL: &str = r#"{"status":"pending","loginId":"L1","events":[{"type":"auth_url","url":"https://x/auth"}]}"#;
    const PENDING_PROMPT: &str = r#"{"status":"pending","loginId":"L1","events":[{"type":"auth_url","url":"https://x/auth"}],"pendingPrompt":{"id":"P1","prompt":{"type":"secret","message":"API key"}}}"#;
    const COMPLETE: &str = r#"{"status":"complete","loginId":"L1","events":[{"type":"auth_url","url":"https://x/auth"},{"type":"progress","message":"done"}]}"#;
    const FAILED: &str =
        r#"{"status":"failed","loginId":"L1","events":[],"error":"login cancelled"}"#;

    #[test]
    fn advance_reports_each_event_once_and_returns_complete() {
        smol::block_on(async {
            let mut login = Login::start(
                client_for(&[PENDING_URL, PENDING_URL, COMPLETE]),
                "anthropic",
                LoginKind::Oauth,
            )
            .await
            .unwrap();
            login.poll_interval = Duration::from_millis(1);
            let mut seen = Vec::new();
            let outcome = login
                .advance(|event| seen.push(format!("{event:?}")), |_| false)
                .await
                .unwrap();
            assert!(matches!(outcome, LoginOutcome::Complete));
            assert_eq!(seen.len(), 2, "{seen:?}");
            assert!(seen[0].contains("https://x/auth"));
        });
    }

    #[test]
    fn advance_returns_a_pending_prompt_and_resumes_after_submit() {
        smol::block_on(async {
            let mut login = Login::start(
                client_for(&[PENDING_PROMPT, COMPLETE]),
                "openai",
                LoginKind::ApiKey,
            )
            .await
            .unwrap();
            login.poll_interval = Duration::from_millis(1);
            let LoginOutcome::Prompt(prompt) = login.advance(|_| {}, |_| false).await.unwrap()
            else {
                panic!("expected a prompt");
            };
            assert_eq!(prompt.id, "P1");
            login.submit("sk-test").await.unwrap();
            assert!(matches!(
                login.advance(|_| {}, |_| false).await.unwrap(),
                LoginOutcome::Complete
            ));
        });
    }

    /// The paste fallback of a browser flow: the box goes up, the loopback
    /// callback wins, and the login must still end complete rather than sit
    /// on a prompt nobody will answer.
    #[test]
    fn a_prompt_the_caller_waits_through_still_completes() {
        smol::block_on(async {
            let mut login = Login::start(
                client_for(&[PENDING_PROMPT, PENDING_PROMPT, COMPLETE]),
                "anthropic",
                LoginKind::Oauth,
            )
            .await
            .unwrap();
            login.poll_interval = Duration::from_millis(1);
            let mut prompts = 0;
            let outcome = login
                .advance(
                    |_| {},
                    |_| {
                        prompts += 1;
                        true
                    },
                )
                .await
                .unwrap();
            assert!(matches!(outcome, LoginOutcome::Complete));
            assert_eq!(prompts, 1, "the same prompt was offered twice");
        });
    }

    #[test]
    fn a_failed_login_is_an_error_with_the_engine_reason() {
        smol::block_on(async {
            let mut login = Login::start(client_for(&[FAILED]), "anthropic", LoginKind::Oauth)
                .await
                .unwrap();
            login.poll_interval = Duration::from_millis(1);
            let error = login.advance(|_| {}, |_| false).await.unwrap_err();
            assert!(
                matches!(&error, LoginError::Failed(reason) if reason == "login cancelled"),
                "{error}"
            );
        });
    }
}
