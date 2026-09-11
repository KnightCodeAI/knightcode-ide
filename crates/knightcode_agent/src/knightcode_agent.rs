//! Seam 1: KnightCode in the native agent's slot.
//!
//! The panel's built-in agent is whatever `Agent::NativeAgent` constructs.
//! This server keeps the native agent's id, so every persisted reference
//! (selected agent, thread rows, drafts) keeps working, and its `connect`
//! spawns `knightcode-engine acp --connect <url>` against the engine this
//! IDE started, through Zed's own `AcpConnection`. The token goes in the
//! child's environment, never on argv.

mod connection;

use acp_thread::AgentConnection;
use agent_client_protocol::schema::v1 as acp;
use agent_servers::{AcpConnection, AgentServer, AgentServerDelegate, CustomAgentServer};
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use fs::Fs;
use gpui::{App, AppContext as _, Entity, Task};
use knightcode_engine::environment::TOKEN_ENV;
use project::{
    AgentId, Project,
    agent_server_store::{AgentServerCommand, AllAgentServersSettings, CustomAgentServerSettings},
};
use settings::{AgentConfigOptionValue, SettingsStore};
use std::{any::Any, path::Path, rc::Rc, sync::Arc};

pub use connection::KnightCodeConnection;

pub struct KnightCodeAgentServer {
    /// Mode, config-option and favourite defaults live under `agent_servers`
    /// keyed by agent id; `CustomAgentServer` already reads and writes them
    /// for any id, so it is reused for those six methods and nothing else.
    settings: CustomAgentServer,
}

impl Default for KnightCodeAgentServer {
    fn default() -> Self {
        Self::new()
    }
}

impl KnightCodeAgentServer {
    pub fn new() -> Self {
        Self {
            settings: CustomAgentServer::new(agent::ZED_AGENT_ID.clone()),
        }
    }
}

fn default_config_options(agent_id: &AgentId, cx: &App) -> HashMap<String, AgentConfigOptionValue> {
    cx.read_global(|settings: &SettingsStore, _| {
        settings
            .get::<AllAgentServersSettings>(None)
            .get(agent_id.as_ref())
            .map(|settings| match settings {
                CustomAgentServerSettings::Custom {
                    default_config_options,
                    ..
                }
                | CustomAgentServerSettings::Registry {
                    default_config_options,
                    ..
                } => default_config_options.clone(),
            })
            .unwrap_or_default()
    })
}

impl AgentServer for KnightCodeAgentServer {
    fn agent_id(&self) -> AgentId {
        agent::ZED_AGENT_ID.clone()
    }

    fn logo(&self) -> ui::IconName {
        ui::IconName::ZedAgent
    }

    fn connect(
        &self,
        _delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let agent_id = self.agent_id();
        let default_mode = self.settings.default_mode(cx);
        let default_config_options = default_config_options(&agent_id, cx);
        let engine = knightcode_engine::global(cx);
        let store = project.read(cx).agent_server_store().downgrade();

        cx.spawn(async move |cx| {
            // Not until the engine is ready: the adapter's token must be one
            // the engine has already accepted, so the panel's auth_required
            // means "no credential", never "wrong token".
            let endpoint = engine
                .update(cx, |engine, cx| engine.ready(cx))
                .await?;
            let (binary, client) = engine.read_with(cx, |engine, _| {
                (engine.binary().map(Path::to_path_buf), engine.client())
            });
            let binary = binary.context(
                "the engine is attached over the network; the agent panel needs a local knightcode-engine",
            )?;
            let login_options = client
                .context("the engine is not ready")?
                .accounts()
                .await?
                .login_options;

            let mut env = HashMap::default();
            env.insert(TOKEN_ENV.to_owned(), endpoint.token.to_string());
            let command = AgentServerCommand {
                path: binary,
                args: vec!["acp".into(), "--connect".into(), endpoint.url.clone()],
                env: Some(env),
            };
            let connection = AcpConnection::stdio(
                agent_id,
                project,
                command,
                store,
                default_mode,
                default_config_options,
                cx,
            )
            .await?;
            Ok(Rc::new(KnightCodeConnection::new(
                Rc::new(connection),
                engine,
                login_options,
            )) as Rc<dyn AgentConnection>)
        })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }

    fn default_mode(&self, cx: &App) -> Option<acp::SessionModeId> {
        self.settings.default_mode(cx)
    }

    fn set_default_mode(&self, mode_id: Option<acp::SessionModeId>, fs: Arc<dyn Fs>, cx: &mut App) {
        self.settings.set_default_mode(mode_id, fs, cx)
    }

    fn default_config_option(&self, config_id: &str, cx: &App) -> Option<AgentConfigOptionValue> {
        self.settings.default_config_option(config_id, cx)
    }

    fn set_default_config_option(
        &self,
        config_id: &str,
        value: Option<AgentConfigOptionValue>,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) {
        self.settings
            .set_default_config_option(config_id, value, fs, cx)
    }

    fn favorite_config_option_value_ids(
        &self,
        config_id: &acp::SessionConfigId,
        cx: &mut App,
    ) -> HashSet<acp::SessionConfigValueId> {
        self.settings
            .favorite_config_option_value_ids(config_id, cx)
    }

    fn toggle_favorite_config_option_value(
        &self,
        config_id: acp::SessionConfigId,
        value_id: acp::SessionConfigValueId,
        should_be_favorite: bool,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) {
        self.settings.toggle_favorite_config_option_value(
            config_id,
            value_id,
            should_be_favorite,
            fs,
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1 as acp;
    use agent_servers::{AgentServerDelegate, connect_fake_acp_connection};
    use fs::FakeFs;
    use gpui::TestAppContext;
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use knightcode_engine::{Endpoint, Engine, LoginKind, LoginOption};
    use project::Project;
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.set_http_client(FakeHttpClient::with_404_response());
        });
    }

    fn options() -> Vec<LoginOption> {
        vec![
            LoginOption {
                provider_id: "anthropic".into(),
                provider_name: "Anthropic".into(),
                kind: LoginKind::Oauth,
                label: "Anthropic (Claude Pro/Max)".into(),
                is_subscription: true,
            },
            LoginOption {
                provider_id: "openai".into(),
                provider_name: "OpenAI".into(),
                kind: LoginKind::ApiKey,
                label: "OpenAI API key".into(),
                is_subscription: false,
            },
        ]
    }

    #[gpui::test]
    async fn connect_surfaces_the_engine_failure_verbatim(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            Engine::failed_for_tests(
                "knightcode-engine was not found at C:/nope: set knightcode.engine_path",
                cx,
            )
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let outcome = cx
            .update(|cx| {
                let delegate = AgentServerDelegate::new(
                    project.read(cx).agent_server_store().clone(),
                    None,
                    None,
                );
                KnightCodeAgentServer::new().connect(delegate, project.clone(), cx)
            })
            .await;
        let error = match outcome {
            Ok(_) => panic!("connect must fail when the engine has failed"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "knightcode-engine was not found at C:/nope: set knightcode.engine_path"
        );
    }

    #[gpui::test]
    async fn the_wrapper_offers_oauth_logins_and_keeps_the_acp_downcast(cx: &mut TestAppContext) {
        init_test(cx);
        let engine = cx.update(|cx| Engine::failed_for_tests("unused", cx));
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let harness = connect_fake_acp_connection(project, cx).await;
        let connection: Rc<dyn AgentConnection> = Rc::new(KnightCodeConnection::new(
            harness.connection.clone(),
            engine,
            options(),
        ));

        let methods = connection.auth_methods();
        assert_eq!(methods.len(), 1, "API-key options are not panel buttons");
        assert_eq!(methods[0].id().0.as_ref(), "knightcode:anthropic");
        assert_eq!(methods[0].name(), "Anthropic (Claude Pro/Max)");
        assert!(connection.clone().downcast::<AcpConnection>().is_some());
        assert!(connection.downcast::<KnightCodeConnection>().is_none());
    }

    #[gpui::test]
    async fn authenticate_opens_the_url_the_engine_reports_and_completes(cx: &mut TestAppContext) {
        init_test(cx);
        let http = FakeHttpClient::create(|request| async move {
            let body = match request.uri().path() {
                "/v1/accounts/login" => r#"{"loginId":"L1"}"#,
                "/v1/accounts/login/L1" => {
                    r#"{"status":"complete","loginId":"L1","events":[{"type":"auth_url","url":"https://x/auth"}]}"#
                }
                _ => "{}",
            };
            Ok(Response::builder()
                .status(200)
                .body(AsyncBody::from(body))
                .unwrap())
        });
        let endpoint = Endpoint {
            url: "http://127.0.0.1:1".into(),
            token: "t".into(),
        };
        let engine = cx.update(|cx| Engine::ready_for_tests(endpoint, http, cx));
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let harness = connect_fake_acp_connection(project, cx).await;
        let connection = KnightCodeConnection::new(harness.connection.clone(), engine, options());

        cx.update(|cx| connection.authenticate(acp::AuthMethodId::new("knightcode:anthropic"), cx))
            .await
            .unwrap();
        assert_eq!(cx.opened_url().as_deref(), Some("https://x/auth"));
    }

    #[test]
    fn no_code_path_can_reach_zeds_credential_provider() {
        // A crate cannot call what it does not link.
        let manifest = include_str!("../Cargo.toml");
        assert!(!manifest.contains("credentials_provider"));
        assert!(!manifest.contains("zed_credentials_provider"));
    }
}
