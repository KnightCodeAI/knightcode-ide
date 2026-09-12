//! Seam 2: every non-panel AI surface — Cmd+K in a buffer, Cmd+K in the
//! terminal, commit messages, thread titles — served by the engine through
//! one `LanguageModelProvider`. The provider holds accounts and models it
//! read from the engine and never a credential; signing in is a login the
//! engine runs, driven from the view in `sign_in.rs`.

pub mod edit_prediction;
mod model;
mod request;
mod sign_in;
mod state;

use gpui::{App, AppContext as _, Entity, Task};
use language_model::{
    AuthenticateError, IconOrSvg, InlineDescription, InlineProviderSettings, LanguageModel,
    LanguageModelProvider, LanguageModelProviderId, LanguageModelProviderName,
    LanguageModelProviderState, ProviderSettingsView, RateLimiter,
};
use std::sync::Arc;
use ui::IconName;

pub use model::KnightCodeLanguageModel;
pub use sign_in::SignInView;
pub use state::{ActiveLogin, LoginStep, State};

pub fn provider_id() -> LanguageModelProviderId {
    LanguageModelProviderId::from("knightcode".to_string())
}

pub fn provider_name() -> LanguageModelProviderName {
    LanguageModelProviderName::from("KnightCode".to_string())
}

pub struct KnightCodeLanguageModelProvider {
    pub state: Entity<State>,
}

impl KnightCodeLanguageModelProvider {
    pub fn new(cx: &mut App) -> Self {
        let engine = knightcode_engine::global(cx);
        let state = cx.new(|cx| State::new(engine, cx));
        Self { state }
    }

    fn model(
        &self,
        model: &knightcode_engine::client::EngineModel,
        cx: &App,
    ) -> Arc<dyn LanguageModel> {
        let state = self.state.read(cx);
        Arc::new(KnightCodeLanguageModel {
            model: model.clone(),
            engine: state.engine(),
            http: cx.http_client(),
            request_limiter: RateLimiter::new(4),
        })
    }
}

impl LanguageModelProviderState for KnightCodeLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for KnightCodeLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        provider_id()
    }

    fn name(&self) -> LanguageModelProviderName {
        provider_name()
    }

    fn icon(&self) -> IconOrSvg {
        IconOrSvg::Icon(IconName::Sparkle)
    }

    fn default_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        let state = self.state.read(cx);
        state.default_model().map(|model| self.model(model, cx))
    }

    fn default_fast_model(&self, _cx: &App) -> Option<Arc<dyn LanguageModel>> {
        None
    }

    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        self.state
            .read(cx)
            .models
            .iter()
            .map(|model| self.model(model, cx))
            .collect()
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        !self.state.read(cx).models.is_empty()
    }

    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        self.state.update(cx, |state, cx| state.refresh(cx))
    }

    fn settings_view(&self, cx: &mut App) -> Option<ProviderSettingsView> {
        let state = self.state.clone();
        let signed_in = self.is_authenticated(cx);
        Some(ProviderSettingsView::Inline(InlineProviderSettings {
            title: (!signed_in).then(|| "Sign in to KnightCode".into()),
            description: (!signed_in).then(|| {
                InlineDescription::Text(
                    "One sign-in serves the agent panel, inline assist, commit messages and edit prediction. The KnightCode CLI shares it."
                        .into(),
                )
            }),
            create_view: Arc::new(move |window, cx| {
                cx.new(|cx| SignInView::new(state.clone(), window, cx))
                    .into()
            }),
        }))
    }

    fn authentication_error_message(&self) -> ui::SharedString {
        "KnightCode's sign-in has expired. Sign in again under Settings > AI > KnightCode.".into()
    }

    fn missing_credentials_error_message(&self) -> ui::SharedString {
        "Sign in to KnightCode under Settings > AI > KnightCode to continue.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use knightcode_engine::{Endpoint, Engine};
    use language_model::AuthenticateError;
    use settings::SettingsStore;
    use std::sync::{Arc, Mutex};

    const NO_ACCOUNTS: &str = r#"{"accounts":[],"loginOptions":[{"providerId":"anthropic","providerName":"Anthropic","type":"oauth","label":"Anthropic (Claude Pro/Max)","isSubscription":true}]}"#;
    const ONE_ACCOUNT: &str = r#"{"accounts":[{"providerId":"anthropic","providerName":"Anthropic","type":"oauth","isSubscription":true}],"loginOptions":[]}"#;
    const NO_MODELS: &str = r#"{"models":[]}"#;
    const TWO_MODELS: &str = r#"{"models":[{"ref":"anthropic/claude-opus-5","id":"claude-opus-5","providerId":"anthropic","providerName":"Anthropic","name":"Claude Opus 5","contextWindow":200000,"maxTokens":32000,"reasoning":true,"input":["text","image"],"cost":{}},{"ref":"agentrouter/claude-opus-5","id":"claude-opus-5","providerId":"agentrouter","providerName":"AgentRouter","name":"Claude Opus 5","contextWindow":200000,"maxTokens":32000,"reasoning":true,"input":["text"],"cost":{}}]}"#;
    /// The catalog as the engine really orders it: alphabetical by provider,
    /// so a provider resolved from an ambient key sorts ahead of the one the
    /// user signed in to.
    const UNCHOSEN_PROVIDER_FIRST: &str = r#"{"models":[{"ref":"agentrouter/claude-opus-4-8","id":"claude-opus-4-8","providerId":"agentrouter","providerName":"AgentRouter","name":"Claude Opus 4.8","contextWindow":200000,"maxTokens":32000,"reasoning":true,"input":["text"],"cost":{}},{"ref":"anthropic/claude-opus-5","id":"claude-opus-5","providerId":"anthropic","providerName":"Anthropic","name":"Claude Opus 5","contextWindow":200000,"maxTokens":32000,"reasoning":true,"input":["text","image"],"cost":{}}]}"#;

    /// A fake engine whose accounts and models can be swapped mid-test.
    fn engine(
        cx: &mut TestAppContext,
        accounts: &'static str,
        models: &'static str,
    ) -> (
        gpui::Entity<Engine>,
        Arc<Mutex<(&'static str, &'static str)>>,
    ) {
        let bodies = Arc::new(Mutex::new((accounts, models)));
        let http = FakeHttpClient::create({
            let bodies = bodies.clone();
            move |request| {
                let bodies = bodies.clone();
                let path = request.uri().path().to_string();
                async move {
                    let (accounts, models) = *bodies.lock().unwrap();
                    let body = match path.as_str() {
                        "/v1/accounts" => accounts,
                        "/v1/models" => models,
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
                }
            }
        });
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.set_http_client(http.clone());
        });
        let endpoint = Endpoint {
            url: "http://127.0.0.1:1".into(),
            token: "t".into(),
        };
        (
            cx.update(|cx| Engine::ready_for_tests(endpoint, http, cx)),
            bodies,
        )
    }

    #[gpui::test]
    async fn unauthenticated_with_no_credential_and_no_key_prompt(cx: &mut TestAppContext) {
        let (_engine, _) = engine(cx, NO_ACCOUNTS, NO_MODELS);
        let provider = cx.update(KnightCodeLanguageModelProvider::new);
        let error = cx.update(|cx| provider.authenticate(cx)).await.unwrap_err();
        assert!(
            matches!(error, AuthenticateError::CredentialsNotFound),
            "{error}"
        );
        cx.update(|cx| {
            assert!(!provider.is_authenticated(cx));
            assert!(provider.provided_models(cx).is_empty());
            assert!(
                matches!(
                    provider.settings_view(cx),
                    Some(ProviderSettingsView::Inline(_))
                ),
                "a sign-in view, not an API-key form"
            );
        });
        assert!(
            cx.opened_url().is_none(),
            "authenticate never opens a browser"
        );
    }

    #[gpui::test]
    async fn authenticated_models_are_qualified_and_disambiguated(cx: &mut TestAppContext) {
        let (_engine, _) = engine(cx, ONE_ACCOUNT, TWO_MODELS);
        let provider = cx.update(KnightCodeLanguageModelProvider::new);
        cx.update(|cx| provider.authenticate(cx)).await.unwrap();
        cx.read(|cx| {
            assert!(provider.is_authenticated(cx));
            let models = provider.provided_models(cx);
            assert_eq!(models.len(), 2);
            assert_eq!(models[0].id().0.as_ref(), "anthropic/claude-opus-5");
            assert_eq!(models[1].id().0.as_ref(), "agentrouter/claude-opus-5");
            assert_ne!(models[0].name(), models[1].name());
            assert_eq!(models[0].max_token_count(), 200_000);
            assert!(models[0].supports_images() && !models[1].supports_images());
            assert!(!models[0].supports_tools());
            assert_eq!(provider.default_model(cx).unwrap().id(), models[0].id());
        });
    }

    #[gpui::test]
    async fn the_default_model_comes_from_an_account_the_user_signed_in_to(
        cx: &mut TestAppContext,
    ) {
        let (_engine, _) = engine(cx, ONE_ACCOUNT, UNCHOSEN_PROVIDER_FIRST);
        let provider = cx.update(KnightCodeLanguageModelProvider::new);
        cx.update(|cx| provider.authenticate(cx)).await.unwrap();
        cx.read(|cx| {
            assert_eq!(
                provider.default_model(cx).unwrap().id().0.as_ref(),
                "anthropic/claude-opus-5",
                "the default must not be whichever provider sorts first"
            );
            // Every model stays offered; only the default is opinionated.
            assert_eq!(provider.provided_models(cx).len(), 2);
        });
    }

    #[gpui::test]
    async fn without_an_account_the_default_is_the_first_model(cx: &mut TestAppContext) {
        // Ambient credentials put models in the catalog with no account row
        // to match; a default is still better than none.
        let (_engine, _) = engine(cx, NO_ACCOUNTS, UNCHOSEN_PROVIDER_FIRST);
        let provider = cx.update(KnightCodeLanguageModelProvider::new);
        cx.update(|cx| provider.authenticate(cx)).await.unwrap();
        cx.read(|cx| {
            assert_eq!(
                provider.default_model(cx).unwrap().id().0.as_ref(),
                "agentrouter/claude-opus-4-8"
            );
        });
    }

    #[gpui::test]
    async fn a_login_opens_the_browser_then_refreshes(cx: &mut TestAppContext) {
        let (_engine, bodies) = engine(cx, NO_ACCOUNTS, NO_MODELS);
        let provider = cx.update(KnightCodeLanguageModelProvider::new);
        cx.update(|cx| provider.authenticate(cx)).await.unwrap_err();
        let option = cx.read(|cx| provider.state.read(cx).login_options[0].clone());
        *bodies.lock().unwrap() = (ONE_ACCOUNT, TWO_MODELS);
        cx.update(|cx| {
            provider
                .state
                .update(cx, |state, cx| state.start_login(option, cx))
        });
        cx.run_until_parked();
        assert_eq!(cx.opened_url().as_deref(), Some("https://x/auth"));
        cx.read(|cx| {
            assert!(
                provider.state.read(cx).login.is_none(),
                "a completed login clears the progress"
            );
            assert!(provider.is_authenticated(cx));
        });
    }

    #[test]
    fn no_code_path_can_reach_zeds_credential_provider() {
        let manifest = include_str!("../Cargo.toml");
        assert!(!manifest.contains("credentials_provider"));
    }
}
