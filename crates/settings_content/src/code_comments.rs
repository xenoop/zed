use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// Settings for the inline code-comments feature.
#[with_fallible_options]
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom, Debug, Default)]
pub struct CodeCommentsSettingsContent {
    /// Whether inline code comments are enabled.
    ///
    /// Default: true
    pub enabled: Option<bool>,
    /// Whether comment cards are drawn in editors.
    ///
    /// Default: true
    pub show_in_editor: Option<bool>,
    /// Configuration for syncing comments with a remote review provider.
    pub sync: Option<CommentSyncSettingsContent>,
}

/// Configuration for the pluggable remote comment-sync layer.
#[with_fallible_options]
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom, Debug, Default)]
pub struct CommentSyncSettingsContent {
    /// Whether remote comment sync is enabled.
    ///
    /// Default: false
    pub enabled: Option<bool>,
    /// Which sync provider to use. The built-in provider is `"custom"`
    /// (driven by the commands configured under `custom`); any provider
    /// registered by another crate can also be named here.
    ///
    /// Default: "custom"
    pub provider: Option<String>,
    /// The remote review unit (a pull-request / merge-request / change id).
    /// Use `"auto"` to detect it from the current branch.
    ///
    /// Default: "auto"
    pub review_unit: Option<String>,
    /// Pull remote comments automatically when a repository is opened.
    ///
    /// Default: true
    pub auto_pull_on_open: Option<bool>,
    /// Configuration for the `"custom"` provider.
    pub custom: Option<CustomCommentSyncSettingsContent>,
}

/// Configuration for the command-driven `"custom"` sync provider, letting a
/// team point comment sync at any internal review system without writing Rust.
#[with_fallible_options]
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom, Debug, Default)]
pub struct CustomCommentSyncSettingsContent {
    /// Command (program followed by arguments) that prints the active review
    /// unit id on stdout, or nothing when no review is active. Run in the
    /// repository root.
    pub detect_command: Option<Vec<String>>,
    /// Command that prints the review unit's comment threads as a JSON array
    /// on stdout. Run in the repository root with the review unit id appended
    /// as its final argument.
    pub fetch_command: Option<Vec<String>>,
}
