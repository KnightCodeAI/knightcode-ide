//! Seam 3: Tab. The Codestral delegate's shape over the engine's
//! `/v1/completions`: a prefix and a suffix around the cursor, one request,
//! the returned text inserted at the cursor and re-interpolated as the user
//! types. The model is a setting, else the provider's default model; no
//! list lives here.
//!
//! Unlike Codestral, typing on does not cancel the request in flight. A
//! model behind the engine can take longer than the gap between keystrokes
//! (grok-4.6 took seven seconds), so cancelling on every key meant no
//! prediction ever landed.

use anyhow::{Result, anyhow};
use edit_prediction::cursor_excerpt;
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger, interpolate_edits,
};
use gpui::{App, Context, Entity, Task, actions};
use icons::IconName;
use knightcode_engine::{Engine, EngineSettings};
use language::{Anchor, Buffer, BufferSnapshot, EditPreview};
use language_model::LanguageModelRegistry;
use settings::Settings as _;
use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};
use text::ToOffset;

const MAX_EDITABLE_TOKENS: usize = 350;
const MAX_CONTEXT_TOKENS: usize = 150;
const MAX_OUTPUT_TOKENS: u32 = 256;
/// The least time between two requests. A burst of keystrokes asks once
/// more, not once per key: every request is billed, answered or not.
const THROTTLE: Duration = Duration::from_millis(300);
/// Requests alive at once: the one in flight and the newest one waiting
/// behind it. A keystroke replaces only the waiting one.
const MAX_PENDING: usize = 2;

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

actions!(
    knightcode,
    [
        /// Opens a picker for the model Tab predictions use.
        SelectEditPredictionModel
    ]
);

/// Whether the user picked a model for Tab, rather than Tab using their
/// chat model.
pub fn edit_prediction_model_is_chosen(cx: &App) -> bool {
    EngineSettings::get_global(cx)
        .edit_prediction_model
        .is_some()
}

/// The name of the model Tab uses, for the Tab menu; its reference when no
/// catalog carries it.
pub fn edit_prediction_model_name(cx: &App) -> Option<String> {
    let reference = edit_prediction_model(cx)?;
    let registry = LanguageModelRegistry::global(cx);
    let name = registry
        .read(cx)
        .provider(&crate::provider_id())
        .and_then(|provider| {
            provider
                .provided_models(cx)
                .into_iter()
                .find(|model| model.id().0 == reference.as_str())
        })
        .map(|model| model.name().0.to_string());
    Some(name.unwrap_or(reference))
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

struct PendingRequest {
    id: usize,
    _task: Task<Result<()>>,
}

pub struct KnightCodeEditPredictionDelegate {
    engine: Entity<Engine>,
    pending: Vec<PendingRequest>,
    next_request_id: usize,
    last_request_at: Option<Instant>,
    current_completion: Option<CurrentCompletion>,
}

impl KnightCodeEditPredictionDelegate {
    pub fn new(cx: &mut App) -> Self {
        Self {
            engine: knightcode_engine::global(cx),
            pending: Vec::new(),
            next_request_id: 0,
            last_request_at: None,
            current_completion: None,
        }
    }

    /// A request is done, and so is every request queued before it: its
    /// answer is at least as fresh as theirs.
    fn finish(&mut self, id: usize) {
        if let Some(ix) = self.pending.iter().position(|request| request.id == id) {
            self.pending.drain(..=ix);
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
        !self.pending.is_empty()
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

        let id = self.next_request_id;
        self.next_request_id += 1;
        let task = cx.spawn(async move |this, cx| {
            if !debounce_duration.is_zero() {
                cx.background_executor().timer(debounce_duration).await;
            }
            let wait = this.update(cx, |this, cx| {
                let now = cx.background_executor().now();
                this.last_request_at
                    .and_then(|at| (at + THROTTLE).checked_duration_since(now))
            })?;
            if let Some(wait) = wait {
                cx.background_executor().timer(wait).await;
            }
            this.update(cx, |this, cx| {
                this.last_request_at = Some(cx.background_executor().now());
            })?;

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
                        this.finish(id);
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
                    this.finish(id);
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
                this.finish(id);
                cx.notify();
            })?;
            Ok(())
        });

        if self.pending.len() >= MAX_PENDING {
            // Dropping a task cancels it. The one given up is the newest
            // waiting, never the one in flight.
            self.pending.pop();
        }
        self.pending.push(PendingRequest { id, _task: task });
    }

    fn accept(&mut self, _cx: &mut Context<Self>) {
        self.pending.clear();
        self.current_completion = None;
    }

    fn discard(&mut self, _reason: EditPredictionDiscardReason, _cx: &mut Context<Self>) {
        self.pending.clear();
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
    use gpui::{AppContext as _, TestAppContext, UpdateGlobal as _};
    use language_model::LanguageModelRegistry;
    use settings::SettingsStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        assert!(cx.read(edit_prediction_model_is_chosen));
        assert_eq!(
            cx.read(edit_prediction_model_name).as_deref(),
            Some("anthropic/claude-haiku-4-5"),
            "no catalog carries it, so the Tab menu shows its reference"
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
        assert_eq!(
            cx.read(edit_prediction_model_name).as_deref(),
            Some("Claude Opus 5")
        );
        assert!(
            !cx.read(edit_prediction_model_is_chosen),
            "the chat model is what Tab falls back to, not a pick"
        );
    }

    /// A model slower than the user's typing still lands a prediction: the
    /// request in flight keeps running while they type, and a burst of
    /// keystrokes asks once more rather than once per key.
    #[gpui::test]
    async fn typing_on_keeps_the_request_in_flight_and_asks_once_more(cx: &mut TestAppContext) {
        const ANSWER_AFTER: Duration = Duration::from_secs(2);
        let requests = Arc::new(AtomicUsize::new(0));
        let executor = cx.executor();
        let http = http_client::FakeHttpClient::create({
            let requests = requests.clone();
            move |request| {
                let requests = requests.clone();
                let executor = executor.clone();
                let is_completion = request.uri().path() == "/v1/completions";
                async move {
                    let body = if is_completion {
                        requests.fetch_add(1, Ordering::SeqCst);
                        executor.timer(ANSWER_AFTER).await;
                        r#"{"choices":[{"text":"println!(\"hi\");"}]}"#
                    } else {
                        "{}"
                    };
                    Ok(http_client::Response::builder()
                        .status(200)
                        .body(http_client::AsyncBody::from(body))
                        .unwrap())
                }
            }
        });
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            knightcode_engine::EngineSettings::register(cx);
            language_model::init(cx);
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .knightcode
                        .get_or_insert_default()
                        .edit_prediction_model = Some("xai/grok-4.6".into());
                });
            });
            let endpoint = knightcode_engine::Endpoint {
                url: "http://127.0.0.1:1".into(),
                token: "t".into(),
            };
            knightcode_engine::Engine::ready_for_tests(endpoint, http, cx);
        });
        let buffer = cx.new(|cx| Buffer::local("fn main() {\n    \n}\n", cx));
        let delegate = cx.new(|cx| KnightCodeEditPredictionDelegate::new(cx));

        let refresh = |offset: usize, cx: &mut TestAppContext| {
            let position = buffer.read_with(cx, |buffer, _| buffer.anchor_after(offset));
            delegate.update(cx, |delegate, cx| {
                delegate.refresh(
                    buffer.clone(),
                    position,
                    Duration::ZERO,
                    EditPredictionRequestTrigger::BufferEdit,
                    cx,
                )
            });
            cx.run_until_parked();
        };
        let type_at = |offset: usize, text: &'static str, cx: &mut TestAppContext| {
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(offset..offset, text)], None, cx)
            });
        };

        let cursor = "fn main() {\n    ".len();
        refresh(cursor, cx);
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        type_at(cursor, "p", cx);
        refresh(cursor + 1, cx);
        type_at(cursor + 1, "r", cx);
        refresh(cursor + 2, cx);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "keystrokes inside the throttle wait rather than ask"
        );

        cx.executor().advance_clock(THROTTLE);
        cx.run_until_parked();
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "the burst asked once more, not once per key"
        );

        cx.executor().advance_clock(ANSWER_AFTER - THROTTLE);
        cx.run_until_parked();
        let position = buffer.read_with(cx, |buffer, _| buffer.anchor_after(cursor + 2));
        let prediction =
            delegate.update(cx, |delegate, cx| delegate.suggest(&buffer, position, cx));
        let Some(EditPrediction::Local { edits, .. }) = prediction else {
            panic!("the first request kept running, so its answer shows");
        };
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].1.as_ref(),
            "intln!(\"hi\");",
            "what the user already typed is not suggested again"
        );
    }

    #[test]
    fn the_completion_is_trimmed_of_code_fences() {
        assert_eq!(clean_completion("```rust\nfoo()\n```"), "foo()");
        assert_eq!(clean_completion("  foo()\n"), "  foo()\n");
        assert_eq!(clean_completion("\n"), "");
    }
}
