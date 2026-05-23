//! Provider-agnostic remote comment sync.
//!
//! The sync layer never mentions GitHub directly: it talks to a
//! [`CommentSyncProvider`] resolved from the [`CommentSyncRegistry`]. Built-in
//! providers cover GitHub and a fully config-driven custom provider, and other
//! crates or extensions can register their own.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use collections::HashMap;
use gpui::{App, AsyncApp, BorrowAppContext as _, Entity, Global};
use serde::{Deserialize, Serialize};
use util::{ResultExt as _, rel_path::RelPath};

use crate::{
    CommentAnchor, CommentAuthor, CommentKind, Comment, ConversationStatus, CommentStore,
    Conversation, ConversationId,
};

/// Git context handed to a sync provider.
#[derive(Clone, Debug)]
pub struct RepoContext {
    /// Absolute path of the repository worktree root; providers run their
    /// tooling (`gh`, custom commands, …) here.
    pub worktree_root: PathBuf,
}

/// An opaque remote review unit — a GitHub PR, GitLab MR, Gerrit change, or
/// any internal review identifier.
#[derive(Clone, Debug)]
pub struct ReviewUnit {
    pub id: String,
}

/// A provider-neutral comment exchanged with a remote backend. Also the JSON
/// shape the `custom` provider's fetch command must emit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteComment {
    pub remote_id: String,
    /// Set for replies; identifies the remote comment this replies to.
    pub parent_remote_id: Option<String>,
    /// Repository-relative file path.
    pub path: String,
    /// Zero-based line the comment is anchored to.
    pub row: u32,
    pub body: String,
    pub author_login: String,
}

/// A locally-authored comment to create on the remote. Also the JSON shape
/// passed to the `custom` provider's push command.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutgoingComment {
    pub parent_remote_id: Option<String>,
    pub path: String,
    pub row: u32,
    pub body: String,
}

/// A pluggable backend for syncing comments with a remote review system.
#[async_trait]
pub trait CommentSyncProvider: Send + Sync + 'static {
    /// Stable provider name, matched against the `provider` setting.
    fn name(&self) -> &str;

    /// Resolves the review unit to sync against. `configured` is the
    /// `review_unit` setting (`"auto"` is passed through as `None`).
    async fn detect_review_unit(
        &self,
        ctx: &RepoContext,
        configured: Option<&str>,
    ) -> Result<Option<ReviewUnit>>;

    /// Fetches every remote comment for the review unit.
    async fn fetch(&self, ctx: &RepoContext, unit: &ReviewUnit) -> Result<Vec<RemoteComment>>;

    /// Creates one comment on the remote, returning its new remote id.
    async fn push(
        &self,
        ctx: &RepoContext,
        unit: &ReviewUnit,
        comment: &OutgoingComment,
    ) -> Result<String>;
}

/// Registry of available sync providers, keyed by [`CommentSyncProvider::name`].
#[derive(Default)]
pub struct CommentSyncRegistry {
    providers: HashMap<String, Arc<dyn CommentSyncProvider>>,
}

impl Global for CommentSyncRegistry {}

impl CommentSyncRegistry {
    /// Registers a provider; other crates and extensions call this to add
    /// support for their own review systems.
    pub fn register(cx: &mut App, provider: Arc<dyn CommentSyncProvider>) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        let name = provider.name().to_string();
        cx.update_global::<Self, _>(|registry, _| {
            registry.providers.insert(name, provider);
        });
    }

    pub fn get(cx: &App, name: &str) -> Option<Arc<dyn CommentSyncProvider>> {
        cx.try_global::<Self>()?.providers.get(name).cloned()
    }
}

/// Runs a pull (fetch remote comments into the store), then pushes any
/// local-only comments back to the remote.
pub async fn sync(
    provider: Arc<dyn CommentSyncProvider>,
    ctx: RepoContext,
    configured_unit: Option<String>,
    store: Entity<CommentStore>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let configured = configured_unit
        .as_deref()
        .filter(|unit| !unit.is_empty() && *unit != "auto");
    let Some(unit) = provider.detect_review_unit(&ctx, configured).await? else {
        log::info!("code_comments: no review unit to sync against");
        return Ok(());
    };

    let remote = provider.fetch(&ctx, &unit).await?;
    let outgoing = merge_remote_comments(&store, remote, cx);

    for comment in outgoing {
        if let Some(remote_id) = provider.push(&ctx, &unit, &comment).await.log_err() {
            log::debug!("code_comments: pushed comment as {remote_id}");
        }
    }
    Ok(())
}

/// Folds fetched remote comments into the store as comment threads, and
/// returns the local-only comments that should be pushed to the remote.
fn merge_remote_comments(
    store: &Entity<CommentStore>,
    remote: Vec<RemoteComment>,
    cx: &mut AsyncApp,
) -> Vec<OutgoingComment> {
    store.update(cx, |store, cx| {
        let known: std::collections::HashSet<i64> = store
            .all_conversations()
            .flat_map(|thread| thread.nodes.iter())
            .filter_map(|node| node.remote_id)
            .collect();

        let roots: Vec<&RemoteComment> = remote
            .iter()
            .filter(|comment| comment.parent_remote_id.is_none())
            .collect();

        for root in roots {
            let Some(root_remote_id) = root.remote_id.parse::<i64>().ok() else {
                continue;
            };
            if known.contains(&root_remote_id) {
                continue;
            }
            let Ok(file) = RelPath::from_proto(&root.path) else {
                continue;
            };

            let root_node = remote_node(root, None);
            let root_node_id = root_node.id;
            let mut nodes = vec![root_node];
            for reply in &remote {
                if reply.parent_remote_id.as_deref() == Some(root.remote_id.as_str()) {
                    nodes.push(remote_node(reply, Some(root_node_id)));
                }
            }

            store.upsert_conversation(
                Conversation {
                    id: ConversationId::new(),
                    change_id: None,
                    file,
                    anchor: CommentAnchor {
                        start_row: root.row,
                        start_column: 0,
                        end_row: root.row,
                        end_column: 0,
                        fingerprint: String::new(),
                        revision: None,
                    },
                    kind: CommentKind::Comment,
                    status: ConversationStatus::Open,
                    nodes,
                    collapsed: false,
                },
                cx,
            );
        }

        // Local-only comments (no remote id) become outgoing pushes.
        let mut outgoing = Vec::new();
        for thread in store.all_conversations() {
            for node in &thread.nodes {
                if node.remote_id.is_some() {
                    continue;
                }
                if !matches!(node.author, CommentAuthor::User) {
                    continue;
                }
                outgoing.push(OutgoingComment {
                    parent_remote_id: None,
                    path: thread.file.to_proto(),
                    row: thread.anchor.start_row,
                    body: node.body.clone(),
                });
            }
        }
        outgoing
    })
}

fn remote_node(comment: &RemoteComment, parent: Option<crate::CommentId>) -> Comment {
    let mut node = Comment::new(
        CommentAuthor::Remote {
            login: comment.author_login.clone(),
        },
        comment.body.clone(),
        parent,
        crate::CommentKind::Comment,
    );
    node.remote_id = comment.remote_id.parse::<i64>().ok();
    node
}
