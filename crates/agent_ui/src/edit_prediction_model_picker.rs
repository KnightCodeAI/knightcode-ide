//! The picker behind the Tab menu's model entry: the model selector the
//! agent panel uses, pointed at `knightcode.edit_prediction_model`.

use std::sync::Arc;

use fs::Fs;
use gpui::{Context, Window};
use knightcode_models::edit_prediction::{SelectEditPredictionModel, edit_prediction_model};
use language_model::{ConfiguredModel, LanguageModelRegistry};
use settings::update_settings_file;
use workspace::Workspace;

use crate::language_model_selector::language_model_selector;

pub fn register(fs: Arc<dyn Fs>, workspace: &mut Workspace) {
    workspace.register_action(
        move |workspace: &mut Workspace,
              _: &SelectEditPredictionModel,
              window: &mut Window,
              cx: &mut Context<Workspace>| {
            let fs = fs.clone();
            workspace.toggle_modal(window, cx, move |window, cx| {
                let focus_handle = cx.focus_handle();
                language_model_selector(
                    |cx| {
                        let reference = edit_prediction_model(cx)?;
                        let provider = LanguageModelRegistry::read_global(cx)
                            .provider(&knightcode_models::provider_id())?;
                        let model = provider
                            .provided_models(cx)
                            .into_iter()
                            .find(|model| model.id().0 == reference.as_str())?;
                        Some(ConfiguredModel { provider, model })
                    },
                    {
                        let fs = fs.clone();
                        move |model, cx| {
                            let reference = model.id().0.to_string();
                            update_settings_file(fs.clone(), cx, move |settings, _| {
                                settings
                                    .knightcode
                                    .get_or_insert_default()
                                    .edit_prediction_model = Some(reference);
                            });
                        }
                    },
                    move |model, should_be_favorite, cx| {
                        crate::favorite_models::toggle_in_settings(
                            model,
                            should_be_favorite,
                            fs.clone(),
                            cx,
                        );
                    },
                    true,
                    focus_handle,
                    window,
                    cx,
                )
            });
        },
    );
}
