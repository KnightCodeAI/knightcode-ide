//! The `knightcode` settings section, resolved.

use settings::{Settings, SettingsContent};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EngineSettings {
    pub engine_path: Option<PathBuf>,
    pub engine_url: Option<String>,
    pub edit_prediction_model: Option<String>,
}

fn non_empty(value: Option<&String>) -> Option<String> {
    value
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

impl Settings for EngineSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let section = content.knightcode.as_ref();
        Self {
            engine_path: non_empty(section.and_then(|section| section.engine_path.as_ref()))
                .map(PathBuf::from),
            engine_url: non_empty(section.and_then(|section| section.engine_url.as_ref())),
            edit_prediction_model: non_empty(
                section.and_then(|section| section.edit_prediction_model.as_ref()),
            ),
        }
    }
}
