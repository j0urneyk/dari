//! App-wide state shared by every window.

use std::path::PathBuf;
use std::sync::Arc;

use dari_net::DeviceIdentity;
use gpui_kit::{App, Global};

use crate::settings::Settings;

pub struct AppState {
    data_directory: PathBuf,
    /// The device identity, or why it could not be loaded.
    identity: Result<Arc<DeviceIdentity>, String>,
    settings: Settings,
}

impl Global for AppState {}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("data_directory", &self.data_directory)
            .finish_non_exhaustive()
    }
}

impl AppState {
    pub fn install(data_directory: PathBuf, cx: &mut App) {
        let identity = DeviceIdentity::load_or_generate(&data_directory)
            .map(Arc::new)
            .map_err(|error| error.to_string());
        let settings = Settings::load(&data_directory);
        crate::text::set_language(settings.language);
        cx.set_global(Self {
            data_directory,
            identity,
            settings,
        });
    }

    pub(crate) fn identity(cx: &App) -> Result<Arc<DeviceIdentity>, String> {
        cx.global::<Self>().identity.clone()
    }

    pub(crate) fn settings(cx: &App) -> &Settings {
        &cx.global::<Self>().settings
    }

    /// Changes settings and saves them; a failed save is logged and the change still applies.
    pub(crate) fn update_settings(cx: &mut App, change: impl FnOnce(&mut Settings)) {
        let state = cx.global_mut::<Self>();
        change(&mut state.settings);
        if let Err(error) = state.settings.save(&state.data_directory) {
            tracing::warn!(%error, "cannot save settings");
        }
    }
}
