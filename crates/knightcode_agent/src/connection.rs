//! `AcpConnection`, with the engine's sign-in in front of it.
//!
//! When `session/new` fails with auth_required the panel offers one button
//! per `auth_methods()` and calls `authenticate` for the one clicked. The
//! adapter's own method is a no-op, so this wrapper offers the engine's
//! OAuth login options instead and drives the login from the IDE: start it,
//! open the URL the engine reports, poll until it completes. Every other
//! method delegates, and `into_any` hands back the inner connection so the
//! one place that downcasts to `AcpConnection` — the ACP log — still finds
//! it.

use acp_thread::{
    AcpThread, AgentConnection, AgentModelSelector, AgentSessionClientUserMessageIds,
    AgentSessionConfigOptions, AgentSessionList, AgentSessionModes, AgentSessionRetry,
    AgentSessionSetTitle, AgentSessionTruncate, AgentTelemetry, ElicitationStore,
};
use agent_client_protocol::schema::v1 as acp;
use agent_servers::AcpConnection;
use anyhow::{Result, anyhow};
use futures::{StreamExt as _, channel::mpsc};
use gpui::{App, AppContext as _, Entity, SharedString, Task};
use knightcode_engine::{
    Engine, LoginKind, LoginOption,
    client::{LoginEvent, PromptKind},
    login::{Login, LoginOutcome},
};
use project::{AgentId, Project};
use std::{any::Any, rc::Rc};
use task::SpawnInTerminal;
use util::path_list::PathList;

pub struct KnightCodeConnection {
    inner: Rc<AcpConnection>,
    engine: Entity<Engine>,
    options: Vec<LoginOption>,
    methods: Vec<acp::AuthMethod>,
}

impl KnightCodeConnection {
    pub fn new(
        inner: Rc<AcpConnection>,
        engine: Entity<Engine>,
        login_options: Vec<LoginOption>,
    ) -> Self {
        // API-key logins need a typed value; those live in Settings > AI.
        let options: Vec<LoginOption> = login_options
            .into_iter()
            .filter(|option| option.kind == LoginKind::Oauth)
            .collect();
        let methods = options
            .iter()
            .map(|option| {
                acp::AuthMethod::Agent(
                    acp::AuthMethodAgent::new(Self::method_id(option), option.label.clone())
                        .description(format!(
                            "Sign in to {} in your browser",
                            option.provider_name
                        )),
                )
            })
            .collect();
        Self {
            inner,
            engine,
            options,
            methods,
        }
    }

    pub fn method_id(option: &LoginOption) -> String {
        format!("knightcode:{}", option.provider_id)
    }
}

impl AgentConnection for KnightCodeConnection {
    fn agent_id(&self) -> AgentId {
        self.inner.agent_id()
    }

    fn telemetry_id(&self) -> SharedString {
        self.inner.telemetry_id()
    }

    fn agent_version(&self) -> Option<SharedString> {
        self.inner.agent_version()
    }

    fn new_session(
        self: Rc<Self>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        self.inner.clone().new_session(project, work_dirs, cx)
    }

    fn supports_load_session(&self) -> bool {
        self.inner.supports_load_session()
    }

    fn load_session(
        self: Rc<Self>,
        session_id: acp::SessionId,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        self.inner
            .clone()
            .load_session(session_id, project, work_dirs, title, cx)
    }

    fn supports_close_session(&self) -> bool {
        self.inner.supports_close_session()
    }

    fn close_session(
        self: Rc<Self>,
        session_id: &acp::SessionId,
        cx: &mut App,
    ) -> Task<Result<()>> {
        self.inner.clone().close_session(session_id, cx)
    }

    fn supports_resume_session(&self) -> bool {
        self.inner.supports_resume_session()
    }

    fn resume_session(
        self: Rc<Self>,
        session_id: acp::SessionId,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        self.inner
            .clone()
            .resume_session(session_id, project, work_dirs, title, cx)
    }

    fn supports_session_history(&self) -> bool {
        self.inner.supports_session_history()
    }

    fn supports_session_additional_directories(&self) -> bool {
        self.inner.supports_session_additional_directories()
    }

    fn auth_methods(&self) -> &[acp::AuthMethod] {
        &self.methods
    }

    fn terminal_auth_task(
        &self,
        _method: &acp::AuthMethodId,
        _cx: &App,
    ) -> Option<Task<Result<SpawnInTerminal>>> {
        None
    }

    fn authenticate(&self, method: acp::AuthMethodId, cx: &mut App) -> Task<Result<()>> {
        let Some(option) = self
            .options
            .iter()
            .find(|option| Self::method_id(option) == method.0.as_ref())
            .cloned()
        else {
            return Task::ready(Err(anyhow!("unknown sign-in method {}", method.0)));
        };
        let Some(client) = self.engine.read(cx).client() else {
            return Task::ready(Err(anyhow!("the engine is not running")));
        };
        cx.spawn(async move |cx| {
            let mut login = Login::start(client, &option.provider_id, option.kind).await?;
            // The login polls in the background and reports events here, where
            // the browser can be opened. Zed shows its own spinner meanwhile.
            let (tx, mut rx) = mpsc::unbounded::<LoginEvent>();
            let polling = cx.background_spawn(async move {
                let outcome = login
                    .advance(
                        |event| {
                            tx.unbounded_send(event.clone()).ok();
                        },
                        // A manual-code prompt is only the paste fallback of a
                        // browser flow; keep waiting for the callback, which is
                        // how these logins finish here. Any other prompt wants
                        // a value this surface has no way to ask for.
                        |prompt| prompt.prompt.kind == PromptKind::ManualCode,
                    )
                    .await;
                (login, outcome)
            });
            while let Some(event) = rx.next().await {
                match event {
                    LoginEvent::AuthUrl { url, .. } => cx.update(|cx| cx.open_url(&url)),
                    LoginEvent::DeviceCode {
                        user_code,
                        verification_uri,
                    } => {
                        log::info!("knightcode: enter code {user_code} at {verification_uri}");
                        cx.update(|cx| cx.open_url(&verification_uri));
                    }
                    LoginEvent::Info { .. } | LoginEvent::Progress { .. } => {}
                }
            }
            let (login, outcome) = polling.await;
            match outcome? {
                LoginOutcome::Complete => Ok(()),
                LoginOutcome::Prompt(_) => {
                    login.cancel().await.ok();
                    Err(anyhow!(
                        "this sign-in needs a value typed in; open Settings > AI > KnightCode to finish it"
                    ))
                }
            }
        })
    }

    fn supports_logout(&self) -> bool {
        self.inner.supports_logout()
    }

    fn logout(&self, cx: &mut App) -> Task<Result<()>> {
        self.inner.logout(cx)
    }

    fn client_user_message_ids(
        &self,
        cx: &App,
    ) -> Option<Rc<dyn AgentSessionClientUserMessageIds>> {
        self.inner.client_user_message_ids(cx)
    }

    fn prompt(
        &self,
        params: acp::PromptRequest,
        cx: &mut App,
    ) -> Task<Result<acp::PromptResponse>> {
        self.inner.prompt(params, cx)
    }

    fn retry(&self, session_id: &acp::SessionId, cx: &App) -> Option<Rc<dyn AgentSessionRetry>> {
        self.inner.retry(session_id, cx)
    }

    fn cancel(&self, session_id: &acp::SessionId, cx: &mut App) {
        self.inner.cancel(session_id, cx)
    }

    fn request_elicitations(&self) -> Option<Entity<ElicitationStore>> {
        self.inner.request_elicitations()
    }

    fn truncate(
        &self,
        session_id: &acp::SessionId,
        cx: &App,
    ) -> Option<Rc<dyn AgentSessionTruncate>> {
        self.inner.truncate(session_id, cx)
    }

    fn set_title(
        &self,
        session_id: &acp::SessionId,
        cx: &App,
    ) -> Option<Rc<dyn AgentSessionSetTitle>> {
        self.inner.set_title(session_id, cx)
    }

    fn model_selector(&self, session_id: &acp::SessionId) -> Option<Rc<dyn AgentModelSelector>> {
        self.inner.model_selector(session_id)
    }

    fn telemetry(&self) -> Option<Rc<dyn AgentTelemetry>> {
        self.inner.telemetry()
    }

    fn session_modes(
        &self,
        session_id: &acp::SessionId,
        cx: &App,
    ) -> Option<Rc<dyn AgentSessionModes>> {
        self.inner.session_modes(session_id, cx)
    }

    fn session_config_options(
        &self,
        session_id: &acp::SessionId,
        cx: &App,
    ) -> Option<Rc<dyn AgentSessionConfigOptions>> {
        self.inner.session_config_options(session_id, cx)
    }

    fn session_list(&self, cx: &mut App) -> Option<Rc<dyn AgentSessionList>> {
        self.inner.session_list(cx)
    }

    /// The inner connection, so `downcast::<AcpConnection>()` succeeds
    /// through the wrapper and the ACP log keeps working.
    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self.inner.clone()
    }
}
