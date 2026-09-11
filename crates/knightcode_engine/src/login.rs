//! One login, driven from the IDE: start it on the engine, poll its state,
//! report each event once (a URL to open, a device code to show), park on a
//! prompt the UI must answer, and end complete or failed. The engine holds
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
    /// The engine needs a value from the user; `submit`, then `advance` again.
    Prompt(PendingPrompt),
}

pub struct Login {
    client: EngineClient,
    pub id: String,
    reported: usize,
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
            poll_interval: Duration::from_millis(500),
        })
    }

    pub async fn advance(
        &mut self,
        mut on_event: impl FnMut(&LoginEvent),
    ) -> Result<LoginOutcome, LoginError> {
        loop {
            let state = self.client.login_state(&self.id).await?;
            let (events, outcome) = match state {
                LoginState::Pending {
                    events,
                    pending_prompt,
                    ..
                } => (events, pending_prompt.map(LoginOutcome::Prompt)),
                LoginState::Complete { events, .. } => (events, Some(LoginOutcome::Complete)),
                LoginState::Failed { error, .. } => return Err(LoginError::Failed(error)),
            };
            for event in events.iter().skip(self.reported) {
                on_event(event);
            }
            self.reported = self.reported.max(events.len());
            if let Some(outcome) = outcome {
                return Ok(outcome);
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
                .advance(|event| seen.push(format!("{event:?}")))
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
            let LoginOutcome::Prompt(prompt) = login.advance(|_| {}).await.unwrap() else {
                panic!("expected a prompt");
            };
            assert_eq!(prompt.id, "P1");
            login.submit("sk-test").await.unwrap();
            assert!(matches!(
                login.advance(|_| {}).await.unwrap(),
                LoginOutcome::Complete
            ));
        });
    }

    #[test]
    fn a_failed_login_is_an_error_with_the_engine_reason() {
        smol::block_on(async {
            let mut login = Login::start(client_for(&[FAILED]), "anthropic", LoginKind::Oauth)
                .await
                .unwrap();
            login.poll_interval = Duration::from_millis(1);
            let error = login.advance(|_| {}).await.unwrap_err();
            assert!(
                matches!(&error, LoginError::Failed(reason) if reason == "login cancelled"),
                "{error}"
            );
        });
    }
}
