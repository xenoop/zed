//! Provider-agnostic code-review integration.
//!
//! The crate doesn't speak to any specific review system directly. It defines
//! a [`CodeReviewProvider`] trait in the vocabulary of the universal data
//! model — changes, revisions, conversations, comments — and lets concrete
//! providers (built-in or extension-supplied) plug in via the
//! [`CodeReviewRegistry`].

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use collections::HashMap;
use gpui::{App, BorrowAppContext as _, Global};

use crate::{ChangeId, Conversation};

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
