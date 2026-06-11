//! Provider-agnostic code-review integration.
//!
//! The crate doesn't speak to any specific review system directly. It defines
//! a [`CodeReviewProvider`] trait in the vocabulary of the universal data
//! model — changes, revisions, conversations, comments — and lets concrete
//! providers (built-in or extension-supplied) plug in via the
//! [`CodeReviewRegistry`].

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use collections::HashMap;
use gpui::{App, BorrowAppContext as _, Global};
use parking_lot::Mutex;
use serde::Deserialize;
use util::rel_path::RelPath;

use crate::{
    ChangeId, Comment, CommentAnchor, CommentAuthor, CommentId, CommentKind, Conversation,
    ConversationId, ConversationStatus, RevisionId,
};

/// Repository context handed to a provider so it can run its own tooling
/// (CLI, HTTP client, …) against the right worktree.
#[derive(Clone, Debug)]
pub struct RepoContext {
    /// Absolute path of the repository worktree root.
    pub worktree_root: PathBuf,
}

/// A pluggable code-review provider. Read-only first: providers expose
/// `detect` (which change is being reviewed in this worktree?) and `fetch`
/// (what conversations exist on that change?). Write methods land in a
/// later iteration once the read path has settled.
#[async_trait]
pub trait CodeReviewProvider: Send + Sync + 'static {
    /// Stable provider name, matched against the `provider` setting and used
    /// when looking up the provider in the registry.
    fn name(&self) -> &str;

    /// Which change (if any) is currently being reviewed in this worktree?
    /// Providers decide how to map the worktree's git state to a change —
    /// most will look at the current branch and consult their backend.
    async fn detect(&self, ctx: &RepoContext) -> Result<Option<ChangeId>>;

    /// All conversations on `change`. Providers return fully-populated
    /// `Conversation` values: each conversation's `change_id` is set to
    /// `change`, and the comment tree inside is linked via
    /// `Comment::in_reply_to` (flat for providers that don't nest, properly
    /// rooted for those that do).
    async fn fetch(
        &self,
        ctx: &RepoContext,
        change: &ChangeId,
    ) -> Result<Vec<Conversation>>;
}

/// Registry of available code-review providers, keyed by
/// [`CodeReviewProvider::name`]. Other crates and extensions call
/// `register` to add their own.
#[derive(Default)]
pub struct CodeReviewRegistry {
    providers: HashMap<String, Arc<dyn CodeReviewProvider>>,
}

impl Global for CodeReviewRegistry {}

impl CodeReviewRegistry {
    pub fn register(cx: &mut App, provider: Arc<dyn CodeReviewProvider>) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        let name = provider.name().to_string();
        cx.update_global::<Self, _>(|registry, _| {
            registry.providers.insert(name, provider);
        });
    }

    pub fn get(cx: &App, name: &str) -> Option<Arc<dyn CodeReviewProvider>> {
        cx.try_global::<Self>()?.providers.get(name).cloned()
    }
}

/// Name under which the command-driven provider registers itself; the value
/// of the `code_comments.sync.provider` setting that selects it.
pub const CUSTOM_PROVIDER_NAME: &str = "custom";

/// The commands the `"custom"` provider runs, taken from settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CustomCommands {
    pub detect: Vec<String>,
    pub fetch: Vec<String>,
}

/// A [`CodeReviewProvider`] driven entirely by user-configured shell commands,
/// so any review system with a CLI can plug in without writing Rust.
///
/// - `detect_command` runs in the worktree root and prints the active change
///   id on stdout (empty output means "no active change").
/// - `fetch_command` runs in the worktree root with the change id appended as
///   its final argument, and prints the change's conversations as JSON (see
///   [`WireConversation`]).
///
/// The commands live behind a mutex because settings can change after the
/// provider is registered; `init` updates them on every settings change.
#[derive(Default)]
pub struct CustomCommandProvider {
    commands: Mutex<CustomCommands>,
}

impl CustomCommandProvider {
    pub fn set_commands(&self, commands: CustomCommands) {
        *self.commands.lock() = commands;
    }

    async fn run(command: &[String], ctx: &RepoContext) -> Result<Option<String>> {
        let Some((program, args)) = command.split_first() else {
            return Ok(None);
        };
        let output = util::command::new_command(program)
            .args(args)
            .current_dir(&ctx.worktree_root)
            .output()
            .await
            .with_context(|| format!("failed to run {program:?}"))?;
        anyhow::ensure!(
            output.status.success(),
            "{program:?} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(Some(String::from_utf8(output.stdout)?))
    }
}

#[async_trait]
impl CodeReviewProvider for CustomCommandProvider {
    fn name(&self) -> &str {
        CUSTOM_PROVIDER_NAME
    }

    async fn detect(&self, ctx: &RepoContext) -> Result<Option<ChangeId>> {
        let command = self.commands.lock().detect.clone();
        let Some(stdout) = Self::run(&command, ctx).await? else {
            return Ok(None);
        };
        let id = stdout.trim().to_string();
        Ok((!id.is_empty()).then(|| ChangeId(id)))
    }

    async fn fetch(&self, ctx: &RepoContext, change: &ChangeId) -> Result<Vec<Conversation>> {
        let mut command = self.commands.lock().fetch.clone();
        if command.is_empty() {
            return Ok(Vec::new());
        }
        command.push(change.0.clone());
        let Some(stdout) = Self::run(&command, ctx).await? else {
            return Ok(Vec::new());
        };
        let wire: Vec<WireConversation> =
            serde_json::from_str(&stdout).context("failed to parse fetch command output")?;
        wire.into_iter()
            .map(|conversation| conversation.into_conversation(change))
            .collect()
    }
}

/// One conversation in the JSON array printed by the custom provider's
/// `fetch_command`. Line numbers are 1-based, matching what editors display.
#[derive(Debug, Deserialize)]
struct WireConversation {
    /// Worktree-relative path, unix-style separators.
    path: String,
    start_line: u32,
    #[serde(default)]
    end_line: Option<u32>,
    /// Trimmed text of the anchored line, used for re-anchoring after edits.
    #[serde(default)]
    line_text: String,
    /// Revision (commit SHA, patchset id, …) the anchor was computed against.
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    resolved: bool,
    comments: Vec<WireComment>,
}

/// One comment within a [`WireConversation`]. `in_reply_to` references the
/// `id` of another comment in the same conversation; comments without it form
/// the root.
#[derive(Debug, Deserialize)]
struct WireComment {
    /// The provider's stable identifier for this comment. Used to match
    /// comments across fetches, so it must not change between runs.
    id: i64,
    #[serde(default)]
    in_reply_to: Option<i64>,
    author: String,
    body: String,
    /// Unix timestamp (seconds).
    #[serde(default)]
    created_at: i64,
}

impl WireConversation {
    fn into_conversation(self, change: &ChangeId) -> Result<Conversation> {
        let file = RelPath::from_proto(&self.path)
            .with_context(|| format!("invalid path {:?} in fetch output", self.path))?;
        let start_row = self.start_line.saturating_sub(1);
        let end_row = self.end_line.unwrap_or(self.start_line).saturating_sub(1);

        let mut id_map: HashMap<i64, CommentId> = HashMap::default();
        for comment in &self.comments {
            id_map.insert(comment.id, CommentId::new());
        }
        let nodes = self
            .comments
            .into_iter()
            .map(|comment| Comment {
                id: id_map[&comment.id],
                in_reply_to: comment
                    .in_reply_to
                    .and_then(|parent| id_map.get(&parent).copied()),
                author: CommentAuthor::Remote {
                    login: comment.author,
                },
                body: comment.body,
                kind: CommentKind::Comment,
                created_at: comment.created_at,
                remote_id: Some(comment.id),
            })
            .collect();

        Ok(Conversation {
            id: ConversationId::new(),
            change_id: Some(change.clone()),
            file,
            anchor: CommentAnchor {
                start_row,
                start_column: 0,
                end_row,
                end_column: 0,
                fingerprint: self.line_text,
                revision: self.revision.map(RevisionId),
            },
            kind: CommentKind::Comment,
            status: if self.resolved {
                ConversationStatus::Resolved
            } else {
                ConversationStatus::Open
            },
            nodes,
            collapsed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wire_conversation_parsing() {
        let json = r#"[{
            "path": "src/main.rs",
            "start_line": 10,
            "line_text": "fn main() {",
            "revision": "abc123",
            "resolved": true,
            "comments": [
                {"id": 1, "author": "alice", "body": "root", "created_at": 100},
                {"id": 2, "in_reply_to": 1, "author": "bob", "body": "reply"}
            ]
        }]"#;
        let wire: Vec<WireConversation> = serde_json::from_str(json).unwrap();
        let conversation = wire
            .into_iter()
            .next()
            .unwrap()
            .into_conversation(&ChangeId("42".into()))
            .unwrap();

        assert_eq!(conversation.change_id, Some(ChangeId("42".into())));
        assert_eq!(conversation.file.as_unix_str(), "src/main.rs");
        assert_eq!(conversation.anchor.start_row, 9);
        assert_eq!(conversation.anchor.end_row, 9);
        assert_eq!(conversation.anchor.fingerprint, "fn main() {");
        assert_eq!(conversation.anchor.revision, Some(RevisionId("abc123".into())));
        assert_eq!(conversation.status, ConversationStatus::Resolved);

        let root = conversation.root().unwrap();
        assert_eq!(root.remote_id, Some(1));
        assert_eq!(
            root.author,
            CommentAuthor::Remote {
                login: "alice".into()
            }
        );
        let reply = conversation.children(root.id).next().unwrap();
        assert_eq!(reply.remote_id, Some(2));
        assert_eq!(reply.in_reply_to, Some(root.id));
    }

    #[test]
    fn test_wire_conversation_rejects_invalid_path() {
        let json = r#"[{"path": "../escape.rs", "start_line": 1, "comments": []}]"#;
        let wire: Vec<WireConversation> = serde_json::from_str(json).unwrap();
        assert!(
            wire.into_iter()
                .next()
                .unwrap()
                .into_conversation(&ChangeId("42".into()))
                .is_err()
        );
    }
}
