//! Runtime view of the `code_comments` settings block.

use settings::{RegisterSetting, Settings, SettingsContent};

/// Resolved `code_comments` settings.
#[derive(Clone, Debug, RegisterSetting)]
pub struct CodeCommentsSettings {
    pub enabled: bool,
    pub show_in_editor: bool,
    pub sync: CommentSyncSettings,
}

/// Resolved remote-sync settings.
#[derive(Clone, Debug)]
pub struct CommentSyncSettings {
    pub enabled: bool,
    /// Name of the sync provider to resolve against the `CommentSyncRegistry`,
    /// or `"custom"` for the command-driven provider.
    pub provider: String,
    /// Explicit review unit id, or `"auto"` to detect it.
    pub review_unit: String,
    pub auto_pull_on_open: bool,
    pub custom_detect_command: Vec<String>,
    pub custom_fetch_command: Vec<String>,
    pub custom_push_command: Vec<String>,
}

impl Settings for CodeCommentsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let content = content.code_comments.clone().unwrap_or_default();
        let sync = content.sync.unwrap_or_default();
        let custom = sync.custom.unwrap_or_default();
        Self {
            enabled: content.enabled.unwrap_or(true),
            show_in_editor: content.show_in_editor.unwrap_or(true),
            sync: CommentSyncSettings {
                enabled: sync.enabled.unwrap_or(false),
                provider: sync.provider.unwrap_or_else(|| "github".to_string()),
                review_unit: sync.review_unit.unwrap_or_else(|| "auto".to_string()),
                auto_pull_on_open: sync.auto_pull_on_open.unwrap_or(true),
                custom_detect_command: custom.detect_command.unwrap_or_default(),
                custom_fetch_command: custom.fetch_command.unwrap_or_default(),
                custom_push_command: custom.push_command.unwrap_or_default(),
            },
        }
    }
}
