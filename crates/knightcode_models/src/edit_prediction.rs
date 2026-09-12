//! Seam 3: Tab. The Codestral delegate's shape over the engine's
//! `/v1/completions`: a prefix and a suffix around the cursor, one request,
//! the returned text inserted at the cursor and re-interpolated as the user
//! types. The model is a setting, else the provider's default model; no
//! list lives here.

use anyhow::{Result, anyhow};
use edit_prediction::cursor_excerpt;
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger, interpolate_edits,
};
use gpui::{App, Context, Entity, Task};
use icons::IconName;
use knightcode_engine::{Engine, EngineSettings};
use language::{Anchor, Buffer, BufferSnapshot, EditPreview};
use language_model::LanguageModelRegistry;
use settings::Settings as _;
use std::{ops::Range, sync::Arc, time::Duration};
use text::ToOffset;

const MAX_EDITABLE_TOKENS: usize = 350;
const MAX_CONTEXT_TOKENS: usize = 150;
const MAX_OUTPUT_TOKENS: u32 = 256;

/// `knightcode.edit_prediction_model`, else the KnightCode provider's
/// default model. `None` until the engine has models.
pub fn edit_prediction_model(cx: &App) -> Option<String> {
    if let Some(model) = &EngineSettings::get_global(cx).edit_prediction_model {
        return Some(model.clone());
    }
    let registry = LanguageModelRegistry::global(cx);
    let provider = registry.read(cx).provider(&crate::provider_id())?;
    Some(provider.default_model(cx)?.id().0.to_string())
}

/// Models sometimes wrap a completion in a fence; the buffer wants code.
fn clean_completion(text: &str) -> String {
    let trimmed = text.trim_matches('\n');
    let unfenced = trimmed
        .strip_prefix("```")
        .map(|rest| rest.split_once('\n').map(|(_, body)| body).unwrap_or(""))
        .map(|body| {
            body.strip_suffix("```")
                .unwrap_or(body)
                .trim_end_matches('\n')
        })
        .unwrap_or(text);
    if unfenced.trim().is_empty() {
        String::new()
    } else {
        unfenced.to_owned()
    }
}

#[derive(Clone)]
struct CurrentCompletion {
    snapshot: BufferSnapshot,
    edits: Arc<[(Range<Anchor>, Arc<str>)]>,
    edit_preview: EditPreview,
}

impl CurrentCompletion {
    fn interpolate(&self, new_snapshot: &BufferSnapshot) -> Option<Vec<(Range<Anchor>, Arc<str>)>> {
        interpolate_edits(&self.snapshot, new_snapshot, &self.edits)
            .filter(|edits| !edits.is_empty())
    }
}

pub struct KnightCodeEditPredictionDelegate {
    engine: Entity<Engine>,
    pending_request: Option<Task<Result<()>>>,
    current_completion: Option<CurrentCompletion>,
}

impl KnightCodeEditPredictionDelegate {
    pub fn new(cx: &mut App) -> Self {
        Self {
            engine: knightcode_engine::global(cx),
            pending_request: None,
            current_completion: None,
        }
    }
}

impl EditPredictionDelegate for KnightCodeEditPredictionDelegate {
    fn name() -> &'static str {
        "knightcode"
    }

    fn display_name() -> &'static str {
        "KnightCode"
    }

    fn show_predictions_in_menu() -> bool {
        true
    }

    fn icons(&self, _cx: &App) -> EditPredictionIconSet {
        EditPredictionIconSet::new(IconName::Sparkle)
    }

    fn is_enabled(&self, _buffer: &Entity<Buffer>, _cursor_position: Anchor, cx: &App) -> bool {
        self.engine.read(cx).endpoint().is_some() && edit_prediction_model(cx).is_some()
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        self.pending_request.is_some()
    }

    fn refresh(
        &mut self,
        buffer: Entity<Buffer>,
        cursor_position: Anchor,
        debounce_duration: Duration,
        _trigger: EditPredictionRequestTrigger,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.engine.read(cx).client() else {
            return;
        };
        let Some(model) = edit_prediction_model(cx) else {
            return;
        };
        let snapshot = buffer.read(cx).snapshot();
        if let Some(current) = &self.current_completion
            && current.interpolate(&snapshot).is_some()
        {
            return;
        }

        self.pending_request = Some(cx.spawn(async move |this, cx| {
            if !debounce_duration.is_zero() {
                cx.background_executor().timer(debounce_duration).await;
            }
            let cursor_offset = cursor_position.to_offset(&snapshot);
            let (excerpt_point_range, excerpt_offset_range, cursor_offset_in_excerpt) =
                cursor_excerpt::compute_cursor_excerpt(&snapshot, cursor_offset);
            let syntax_ranges = cursor_excerpt::compute_syntax_ranges(
                &snapshot,
                cursor_offset,
                &excerpt_offset_range,
            );
            let excerpt_text: String = snapshot.text_for_range(excerpt_point_range).collect();
            let (_, context_range) = zeta_prompt::compute_editable_and_context_ranges(
                &excerpt_text,
                cursor_offset_in_excerpt,
                &syntax_ranges,
                MAX_EDITABLE_TOKENS,
                MAX_CONTEXT_TOKENS,
            );
            let context_text = &excerpt_text[context_range.clone()];
            let cursor = cursor_offset_in_excerpt
                .saturating_sub(context_range.start)
                .min(context_text.len());
            let prefix = context_text[..cursor].to_string();
            let suffix = context_text[cursor..].to_string();

            let text = match client
                .completion(&model, &prefix, &suffix, MAX_OUTPUT_TOKENS)
                .await
            {
                Ok(text) => clean_completion(&text),
                Err(error) => {
                    log::warn!("knightcode: edit prediction from {model} failed: {error}");
                    this.update(cx, |this, cx| {
                        this.pending_request = None;
                        cx.notify();
                    })?;
                    return Err(anyhow!(error));
                }
            };
            // Nothing else records a prediction that worked, which makes a
            // silent log ambiguous: served, or never asked for?
            log::debug!(
                "knightcode: edit prediction from {model}: {} characters",
                text.len()
            );
            if text.is_empty() {
                this.update(cx, |this, cx| {
                    this.pending_request = None;
                    cx.notify();
                })?;
                return Ok(());
            }

            let edits: Arc<[(Range<Anchor>, Arc<str>)]> =
                vec![(cursor_position..cursor_position, text.into())].into();
            let edit_preview = buffer
                .read_with(cx, |buffer, cx| buffer.preview_edits(edits.clone(), cx))
                .await;
            this.update(cx, |this, cx| {
                this.current_completion = Some(CurrentCompletion {
                    snapshot,
                    edits,
                    edit_preview,
                });
                this.pending_request = None;
                cx.notify();
            })?;
            Ok(())
        }));
    }

    fn accept(&mut self, _cx: &mut Context<Self>) {
        self.pending_request = None;
        self.current_completion = None;
    }

    fn discard(&mut self, _reason: EditPredictionDiscardReason, _cx: &mut Context<Self>) {
        self.pending_request = None;
        self.current_completion = None;
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        _cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        let current = self.current_completion.as_ref()?;
        let edits = current.interpolate(&buffer.read(cx).snapshot())?;
        Some(EditPrediction::Local {
            id: None,
            edits,
            cursor_position: None,
            edit_preview: Some(current.edit_preview.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use language_model::LanguageModelRegistry;
    use settings::SettingsStore;

    #[gpui::test]
    fn the_model_is_the_setting_then_the_providers_default(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            knightcode_engine::EngineSettings::register(cx);
            language_model::init(cx);
        });
        assert_eq!(
            cx.read(edit_prediction_model),
            None,
            "no setting, no provider"
        );

        cx.update(|cx| {
            let registry = LanguageModelRegistry::global(cx);
            registry.update(cx, |registry, cx| {
                registry.register_provider(
                    Arc::new(language_model::fake_provider::FakeLanguageModelProvider::default()),
                    cx,
                )
            });
        });
        // Only KnightCode's provider counts: a fake provider under another id is ignored.
        assert_eq!(cx.read(edit_prediction_model), None);

        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .knightcode
                        .get_or_insert_default()
                        .edit_prediction_model = Some("anthropic/claude-haiku-4-5".into());
                });
            });
        });
        assert_eq!(
            cx.read(edit_prediction_model).as_deref(),
            Some("anthropic/claude-haiku-4-5")
        );
    }

    /// With no `edit_prediction_model` of its own, Tab uses what the user
    /// chose in the CLI — which the engine reports — and nothing otherwise.
    #[gpui::test]
    async fn without_a_setting_the_model_is_the_one_the_user_chose(cx: &mut TestAppContext) {
        let http = http_client::FakeHttpClient::create(|request| async move {
            let body = match request.uri().path() {
                "/v1/accounts" => {
                    r#"{"accounts":[{"providerId":"anthropic","providerName":"Anthropic","type":"oauth","isSubscription":true}],"loginOptions":[]}"#
                }
                "/v1/models" => {
                    r#"{"models":[{"ref":"anthropic/claude-opus-5","id":"claude-opus-5","providerId":"anthropic","providerName":"Anthropic","name":"Claude Opus 5","contextWindow":200000,"maxTokens":32000,"reasoning":true,"input":["text"],"cost":{}}],"default":"anthropic/claude-opus-5"}"#
                }
                _ => "{}",
            };
            Ok(http_client::Response::builder()
                .status(200)
                .body(http_client::AsyncBody::from(body))
                .unwrap())
        });
        let provider = cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            knightcode_engine::EngineSettings::register(cx);
            language_model::init(cx);
            let endpoint = knightcode_engine::Endpoint {
                url: "http://127.0.0.1:1".into(),
                token: "t".into(),
            };
            knightcode_engine::Engine::ready_for_tests(endpoint, http, cx);
            let provider = Arc::new(crate::KnightCodeLanguageModelProvider::new(cx));
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.register_provider(provider.clone(), cx)
            });
            provider
        });
        assert_eq!(cx.read(edit_prediction_model), None, "no models yet");
        cx.update(|cx| {
            use language_model::LanguageModelProvider as _;
            provider.authenticate(cx)
        })
        .await
        .unwrap();
        assert_eq!(
            cx.read(edit_prediction_model).as_deref(),
            Some("anthropic/claude-opus-5"),
            "the model the engine reported as the user's, by its reference"
        );
    }

    #[test]
    fn the_completion_is_trimmed_of_code_fences() {
        assert_eq!(clean_completion("```rust\nfoo()\n```"), "foo()");
        assert_eq!(clean_completion("  foo()\n"), "  foo()\n");
        assert_eq!(clean_completion("\n"), "");
    }
}
