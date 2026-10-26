use std::{ops::RangeInclusive, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use gpui::SharedString;
use http_client::HttpClient;
use time::OffsetDateTime;
use url::Url;

use crate::{ParsedGitRemote, repository::RepoPath};

/// What a code review host (GitHub, GitLab, ...) supports, so the UI can adapt
/// without special-casing hosts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReviewCapabilities {
    /// Comments can reply to a specific comment rather than to the thread.
    pub nested_replies: bool,
    pub resolve_threads: bool,
    pub suggested_changes: bool,
    /// Reviews can carry a verdict (approve / request changes).
    pub verdicts: bool,
}

/// One place a provider may find an API token, tried in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    /// Run a command whose trimmed stdout is the token, e.g. `gh auth token`.
    Command {
        program: SharedString,
        args: Vec<SharedString>,
    },
    EnvironmentVariable(SharedString),
    /// The OS keychain, under the provider's `credential_key`.
    Keychain,
}

impl CredentialSource {
    pub fn label(&self) -> SharedString {
        match self {
            CredentialSource::Command { program, args } => {
                let mut label = program.to_string();
                for arg in args.iter().take_while(|arg| !arg.starts_with('-')) {
                    label.push(' ');
                    label.push_str(arg);
                }
                label.into()
            }
            CredentialSource::EnvironmentVariable(name) => name.clone(),
            CredentialSource::Keychain => "Keychain".into(),
        }
    }
}

/// Everything a provider needs to make an authenticated request.
#[derive(Clone)]
pub struct ReviewRequestContext {
    pub http_client: Arc<dyn HttpClient>,
    pub token: String,
}

/// Identifies which review (pull request, merge request, ...) to look for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewQuery {
    /// The repository the review was opened against.
    pub base_repository: ParsedGitRemote,
    /// The owner of the repository holding the head branch. Differs from the
    /// base repository's owner when the branch lives in a fork.
    pub head_owner: Arc<str>,
    pub head_branch: SharedString,
    pub base_branch: Option<SharedString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    /// The provider's own identifier, used for follow-up requests.
    pub id: SharedString,
    pub number: u64,
    pub title: SharedString,
    pub url: Url,
    pub base_repository: ParsedGitRemote,
    pub head_sha: SharedString,
    pub base_ref: SharedString,
    pub author_login: SharedString,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadId(pub SharedString);

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommentId(pub SharedString);

/// Which side of the diff a thread is attached to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSide {
    /// The base version (removed lines).
    Old,
    /// The head version (added or unchanged lines).
    New,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentAuthorRole {
    /// Opened the review.
    Author,
    Reviewer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentAuthor {
    pub login: SharedString,
    pub avatar_url: Option<Url>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewComment {
    pub id: CommentId,
    pub author: Option<CommentAuthor>,
    pub role: CommentAuthorRole,
    /// Markdown.
    pub body: SharedString,
    pub created_at: OffsetDateTime,
    /// Set only by hosts with nested replies.
    pub in_reply_to: Option<CommentId>,
    pub url: Option<Url>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewThread {
    pub id: ThreadId,
    pub path: RepoPath,
    pub side: DiffSide,
    /// 1-based lines in the review's head version, or `None` when the code the
    /// thread was written on no longer exists there.
    pub lines: Option<RangeInclusive<u32>>,
    /// 1-based lines in the version the thread was written on.
    pub original_lines: Option<RangeInclusive<u32>>,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub can_resolve: bool,
    pub can_reply: bool,
    pub resolved_by: Option<SharedString>,
    /// In the order the host returns them, which is creation order.
    pub comments: Vec<ReviewComment>,
}

impl ReviewThread {
    pub fn first_comment(&self) -> Option<&ReviewComment> {
        self.comments.first()
    }

    /// The comments with how deeply each one is nested. Comments whose parent
    /// is missing are treated as replies to the thread.
    pub fn comments_with_depth(&self) -> Vec<(&ReviewComment, usize)> {
        let mut depths: Vec<(&CommentId, usize)> = Vec::with_capacity(self.comments.len());
        self.comments
            .iter()
            .enumerate()
            .map(|(index, comment)| {
                let depth = if index == 0 {
                    0
                } else {
                    comment
                        .in_reply_to
                        .as_ref()
                        .and_then(|parent| {
                            depths
                                .iter()
                                .find(|(id, _)| *id == parent)
                                .map(|(_, parent_depth)| parent_depth + 1)
                        })
                        .unwrap_or(1)
                };
                depths.push((&comment.id, depth));
                (comment, depth)
            })
            .collect()
    }
}

/// A host that serves code reviews: finding the review for a branch and
/// reading its threads. Implemented once per host; the UI only talks to this.
#[async_trait]
pub trait CodeReviewProvider: Send + Sync {
    fn name(&self) -> SharedString;

    fn capabilities(&self) -> ReviewCapabilities;

    /// The key the token is stored under in the keychain.
    fn credential_key(&self) -> String;

    fn credential_sources(&self) -> Vec<CredentialSource>;

    async fn find_review_for_branch(
        &self,
        request: &ReviewRequestContext,
        query: &ReviewQuery,
    ) -> Result<Option<Review>>;

    async fn load_threads(
        &self,
        request: &ReviewRequestContext,
        review: &Review,
    ) -> Result<Vec<ReviewThread>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: &str, in_reply_to: Option<&str>) -> ReviewComment {
        ReviewComment {
            id: CommentId(id.to_string().into()),
            author: None,
            role: CommentAuthorRole::Reviewer,
            body: "".into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            in_reply_to: in_reply_to.map(|id| CommentId(id.to_string().into())),
            url: None,
        }
    }

    fn thread(comments: Vec<ReviewComment>) -> ReviewThread {
        ReviewThread {
            id: ThreadId("thread".into()),
            path: RepoPath::new("src/main.rs").unwrap(),
            side: DiffSide::New,
            lines: Some(1..=1),
            original_lines: Some(1..=1),
            is_resolved: false,
            is_outdated: false,
            can_resolve: true,
            can_reply: true,
            resolved_by: None,
            comments,
        }
    }

    #[test]
    fn test_comment_depths() {
        let thread = thread(vec![
            comment("root", None),
            comment("flat", None),
            comment("reply-to-root", Some("root")),
            comment("nested", Some("reply-to-root")),
            comment("orphan", Some("missing")),
        ]);
        let depths = thread
            .comments_with_depth()
            .into_iter()
            .map(|(comment, depth)| (comment.id.0.to_string(), depth))
            .collect::<Vec<_>>();
        assert_eq!(
            depths,
            vec![
                ("root".to_string(), 0),
                ("flat".to_string(), 1),
                ("reply-to-root".to_string(), 1),
                ("nested".to_string(), 2),
                ("orphan".to_string(), 1),
            ]
        );
    }

    #[test]
    fn test_credential_source_labels() {
        let command = CredentialSource::Command {
            program: "gh".into(),
            args: vec!["auth".into(), "token".into(), "--hostname".into()],
        };
        assert_eq!(command.label().as_ref(), "gh auth token");
        assert_eq!(
            CredentialSource::EnvironmentVariable("GITHUB_TOKEN".into())
                .label()
                .as_ref(),
            "GITHUB_TOKEN"
        );
    }
}
