use std::sync::Arc;

use anyhow::{Context as _, Result};
use credentials_provider::CredentialsProvider;
use futures::future::BoxFuture;
use git::{
    GitHostingProviderRegistry,
    code_review::{
        CodeReviewProvider, CredentialSource, Review, ReviewCapabilities, ReviewQuery,
        ReviewRequestContext, ReviewThread,
    },
    parse_git_remote_url,
};
use gpui::{App, AsyncApp, Context, EventEmitter, SharedString, Task};
use http_client::HttpClient;
use util::ResultExt as _;

/// The parts of a repository's state needed to find its review.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryReviewInfo {
    pub origin_url: Option<String>,
    pub upstream_url: Option<String>,
    pub branch: Option<SharedString>,
    /// The branch's name on its remote, when it tracks one.
    pub remote_branch: Option<SharedString>,
    pub head_sha: Option<SharedString>,
}

#[derive(Clone)]
pub struct ReviewTarget {
    pub provider: Arc<dyn CodeReviewProvider>,
    pub query: ReviewQuery,
    pub local_head_sha: Option<SharedString>,
}

impl std::fmt::Debug for ReviewTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewTarget")
            .field("provider", &self.provider.name())
            .field("query", &self.query)
            .field("local_head_sha", &self.local_head_sha)
            .finish()
    }
}

/// Works out which host and review a repository's branch belongs to.
///
/// Reviews live on the upstream repository when there is one (a fork
/// workflow), while the branch itself is pushed to origin.
pub fn review_target(
    info: &RepositoryReviewInfo,
    base_ref: &str,
    registry: Arc<GitHostingProviderRegistry>,
) -> Result<ReviewTarget, SharedString> {
    let base_remote_url = info
        .upstream_url
        .as_deref()
        .or(info.origin_url.as_deref())
        .ok_or_else(|| SharedString::from("This repository has no remote."))?;
    let (hosting_provider, base_repository) =
        parse_git_remote_url(registry.clone(), base_remote_url).ok_or_else(|| {
            SharedString::from("Review threads aren't available for this remote's host.")
        })?;
    let provider = hosting_provider.code_review_provider().ok_or_else(|| {
        SharedString::from(format!(
            "Review threads aren't supported for {} yet.",
            hosting_provider.name()
        ))
    })?;
    let head_owner = info
        .origin_url
        .as_deref()
        .and_then(|origin_url| parse_git_remote_url(registry, origin_url))
        .map(|(_, origin)| origin.owner)
        .unwrap_or_else(|| base_repository.owner.clone());
    let head_branch = info
        .remote_branch
        .clone()
        .or_else(|| info.branch.clone())
        .ok_or_else(|| SharedString::from("Check out a branch to see its review threads."))?;

    Ok(ReviewTarget {
        provider,
        query: ReviewQuery {
            base_repository,
            head_owner,
            head_branch,
            base_branch: branch_name_from_ref(base_ref),
        },
        local_head_sha: info.head_sha.clone(),
    })
}

/// Turns a ref like `origin/main` or `refs/heads/main` into `main`.
fn branch_name_from_ref(base_ref: &str) -> Option<SharedString> {
    let name = base_ref
        .strip_prefix("refs/heads/")
        .or_else(|| {
            base_ref
                .strip_prefix("refs/remotes/")
                .and_then(|rest| rest.split_once('/').map(|(_, name)| name))
        })
        .or_else(|| base_ref.strip_prefix("origin/"))
        .or_else(|| base_ref.strip_prefix("upstream/"))
        .unwrap_or(base_ref);
    (!name.is_empty()).then(|| name.to_string().into())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialOutcome {
    Found,
    NotFound,
    Failed(SharedString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialAttempt {
    pub source: CredentialSource,
    pub outcome: CredentialOutcome,
}

/// Where credential lookups read environment variables and run commands.
/// Swapped out in tests.
pub trait CredentialEnvironment: Send + Sync {
    fn environment_variable(&self, name: &str) -> Option<String>;

    /// Runs a command and returns its trimmed stdout when it succeeds.
    fn run_command(&self, program: String, args: Vec<String>)
    -> BoxFuture<'static, Result<String>>;
}

pub struct SystemCredentialEnvironment;

impl CredentialEnvironment for SystemCredentialEnvironment {
    fn environment_variable(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn run_command(
        &self,
        program: String,
        args: Vec<String>,
    ) -> BoxFuture<'static, Result<String>> {
        Box::pin(async move {
            let output = util::command::new_command(&program)
                .args(&args)
                .output()
                .await
                .with_context(|| format!("running {program}"))?;
            anyhow::ensure!(
                output.status.success(),
                "{program} exited with {}",
                output.status
            );
            Ok(String::from_utf8(output.stdout)?.trim().to_string())
        })
    }
}

/// Tries each of the provider's credential sources in order and returns the
/// first token found, along with what every attempt found.
pub async fn resolve_token(
    provider: &dyn CodeReviewProvider,
    credentials_provider: &dyn CredentialsProvider,
    environment: &dyn CredentialEnvironment,
    cx: &AsyncApp,
) -> (Option<String>, Vec<CredentialAttempt>) {
    let mut attempts = Vec::new();
    for source in provider.credential_sources() {
        let result = match &source {
            CredentialSource::Command { program, args } => environment
                .run_command(
                    program.to_string(),
                    args.iter().map(|arg| arg.to_string()).collect(),
                )
                .await
                .map(Some),
            CredentialSource::EnvironmentVariable(name) => {
                Ok(environment.environment_variable(name))
            }
            CredentialSource::Keychain => credentials_provider
                .read_credentials(&provider.credential_key(), cx)
                .await
                .and_then(|credentials| {
                    credentials
                        .map(|(_, password)| String::from_utf8(password))
                        .transpose()
                        .map_err(Into::into)
                }),
        };
        let token = match result {
            Ok(token) => token.filter(|token| !token.trim().is_empty()),
            Err(error) => {
                attempts.push(CredentialAttempt {
                    source,
                    outcome: CredentialOutcome::Failed(format!("{error:#}").into()),
                });
                continue;
            }
        };
        match token {
            Some(token) => {
                attempts.push(CredentialAttempt {
                    source,
                    outcome: CredentialOutcome::Found,
                });
                return (Some(token.trim().to_string()), attempts);
            }
            None => attempts.push(CredentialAttempt {
                source,
                outcome: CredentialOutcome::NotFound,
            }),
        }
    }
    (None, attempts)
}

#[derive(Clone, Debug)]
pub struct LoadedReview {
    pub review: Review,
    pub threads: Vec<ReviewThread>,
    pub capabilities: ReviewCapabilities,
    /// Whether the local checkout is at the review's head commit, so thread
    /// line numbers line up with the files on disk.
    pub local_head_matches: bool,
}

impl LoadedReview {
    pub fn open_thread_count(&self) -> usize {
        self.threads
            .iter()
            .filter(|thread| !thread.is_resolved)
            .count()
    }
}

#[derive(Clone, Debug, Default)]
pub enum ReviewState {
    #[default]
    Idle,
    Loading,
    /// Review threads can't be shown for this repository, with the reason.
    Unavailable(SharedString),
    SignedOut {
        provider_name: SharedString,
        attempts: Vec<CredentialAttempt>,
    },
    NoReview {
        head_branch: SharedString,
        base_branch: Option<SharedString>,
    },
    Loaded(LoadedReview),
    Failed(SharedString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewStoreEvent {
    Changed,
}

/// Holds the review threads for one repository's current branch.
pub struct ReviewStore {
    state: ReviewState,
    target: Option<ReviewTarget>,
    is_refreshing: bool,
    http_client: Arc<dyn HttpClient>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    environment: Arc<dyn CredentialEnvironment>,
    refresh_task: Option<Task<()>>,
}

impl EventEmitter<ReviewStoreEvent> for ReviewStore {}

impl ReviewStore {
    pub fn new(cx: &App) -> Self {
        Self::with_dependencies(
            cx.http_client(),
            zed_credentials_provider::global(cx),
            Arc::new(SystemCredentialEnvironment),
        )
    }

    pub fn with_dependencies(
        http_client: Arc<dyn HttpClient>,
        credentials_provider: Arc<dyn CredentialsProvider>,
        environment: Arc<dyn CredentialEnvironment>,
    ) -> Self {
        Self {
            state: ReviewState::Idle,
            target: None,
            is_refreshing: false,
            http_client,
            credentials_provider,
            environment,
            refresh_task: None,
        }
    }

    pub fn state(&self) -> &ReviewState {
        &self.state
    }

    pub fn loaded(&self) -> Option<&LoadedReview> {
        match &self.state {
            ReviewState::Loaded(loaded) => Some(loaded),
            _ => None,
        }
    }

    pub fn is_refreshing(&self) -> bool {
        self.is_refreshing
    }

    pub fn set_unavailable(&mut self, reason: SharedString, cx: &mut Context<Self>) {
        self.target = None;
        self.refresh_task = None;
        self.is_refreshing = false;
        self.set_state(ReviewState::Unavailable(reason), cx);
    }

    /// Loads the review for `target`, keeping the current threads on screen
    /// while a reload of the same review is in flight.
    pub fn refresh(&mut self, target: ReviewTarget, cx: &mut Context<Self>) {
        let same_target = self
            .target
            .as_ref()
            .is_some_and(|current| current.query == target.query);
        if !same_target || self.loaded().is_none() {
            self.state = ReviewState::Loading;
        }
        self.target = Some(target.clone());
        self.is_refreshing = true;
        cx.emit(ReviewStoreEvent::Changed);
        cx.notify();

        let http_client = self.http_client.clone();
        let credentials_provider = self.credentials_provider.clone();
        let environment = self.environment.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let state =
                load_review_state(target, http_client, credentials_provider, environment, cx).await;
            this.update(cx, |this, cx| {
                this.is_refreshing = false;
                this.set_state(state, cx);
            })
            .log_err();
        }));
    }

    /// Reloads the last requested review, if any.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = self.target.clone() {
            self.refresh(target, cx);
        }
    }

    /// Saves a token to the keychain for the current provider, then reloads.
    pub fn save_token(&mut self, token: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(target) = self.target.clone() else {
            return Task::ready(Err(anyhow::anyhow!("no review host to save a token for")));
        };
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |this, cx| {
            let key = target.provider.credential_key();
            credentials_provider
                .write_credentials(&key, "token", token.trim().as_bytes(), cx)
                .await?;
            this.update(cx, |this, cx| this.refresh(target, cx))?;
            Ok(())
        })
    }

    fn set_state(&mut self, state: ReviewState, cx: &mut Context<Self>) {
        self.state = state;
        cx.emit(ReviewStoreEvent::Changed);
        cx.notify();
    }
}

async fn load_review_state(
    target: ReviewTarget,
    http_client: Arc<dyn HttpClient>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    environment: Arc<dyn CredentialEnvironment>,
    cx: &mut AsyncApp,
) -> ReviewState {
    let provider = target.provider.clone();
    let (token, attempts) = resolve_token(
        provider.as_ref(),
        credentials_provider.as_ref(),
        environment.as_ref(),
        cx,
    )
    .await;
    let Some(token) = token else {
        return ReviewState::SignedOut {
            provider_name: provider.name(),
            attempts,
        };
    };

    let request = ReviewRequestContext { http_client, token };
    let query = target.query.clone();
    let result = cx
        .background_executor()
        .spawn(async move {
            let Some(review) = provider.find_review_for_branch(&request, &query).await? else {
                return anyhow::Ok(None);
            };
            let threads = provider.load_threads(&request, &review).await?;
            Ok(Some((review, threads, provider.capabilities())))
        })
        .await;

    match result {
        Ok(Some((review, threads, capabilities))) => {
            let local_head_matches = target
                .local_head_sha
                .as_ref()
                .is_some_and(|sha| *sha == review.head_sha);
            ReviewState::Loaded(LoadedReview {
                review,
                threads,
                capabilities,
                local_head_matches,
            })
        }
        Ok(None) => ReviewState::NoReview {
            head_branch: target.query.head_branch,
            base_branch: target.query.base_branch,
        },
        Err(error) => ReviewState::Failed(format!("{error:#}").into()),
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod fake {
    use super::*;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::collections::HashMap;

    /// A review host whose reviews and threads are set by the test.
    #[derive(Default)]
    pub struct FakeCodeReviewProvider {
        pub capabilities: ReviewCapabilities,
        pub credential_sources: Vec<CredentialSource>,
        pub state: Mutex<FakeCodeReviewState>,
    }

    #[derive(Default)]
    pub struct FakeCodeReviewState {
        pub reviews_by_branch: HashMap<SharedString, Review>,
        pub threads: Vec<ReviewThread>,
        pub fail_with: Option<String>,
        pub tokens_seen: Vec<String>,
    }

    #[async_trait]
    impl CodeReviewProvider for FakeCodeReviewProvider {
        fn name(&self) -> SharedString {
            "Fake".into()
        }

        fn capabilities(&self) -> ReviewCapabilities {
            self.capabilities
        }

        fn credential_key(&self) -> String {
            "https://fake.example".into()
        }

        fn credential_sources(&self) -> Vec<CredentialSource> {
            self.credential_sources.clone()
        }

        async fn find_review_for_branch(
            &self,
            request: &ReviewRequestContext,
            query: &ReviewQuery,
        ) -> Result<Option<Review>> {
            let mut state = self.state.lock();
            state.tokens_seen.push(request.token.clone());
            if let Some(error) = &state.fail_with {
                anyhow::bail!("{error}");
            }
            Ok(state.reviews_by_branch.get(&query.head_branch).cloned())
        }

        async fn load_threads(
            &self,
            _request: &ReviewRequestContext,
            _review: &Review,
        ) -> Result<Vec<ReviewThread>> {
            Ok(self.state.lock().threads.clone())
        }
    }

    pub struct FakeCredentialEnvironment {
        pub variables: HashMap<String, String>,
        pub command_output: Option<String>,
    }

    impl CredentialEnvironment for FakeCredentialEnvironment {
        fn environment_variable(&self, name: &str) -> Option<String> {
            self.variables.get(name).cloned()
        }

        fn run_command(
            &self,
            program: String,
            _args: Vec<String>,
        ) -> BoxFuture<'static, Result<String>> {
            let output = self.command_output.clone();
            Box::pin(async move { output.with_context(|| format!("{program} is not signed in")) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        fake::{FakeCodeReviewProvider, FakeCredentialEnvironment},
        *,
    };
    use credentials_provider::test_support::FakeCredentialsProvider;
    use git::{
        ParsedGitRemote,
        code_review::{CommentAuthorRole, CommentId, DiffSide, ReviewComment, ThreadId},
        repository::RepoPath,
    };
    use gpui::{AppContext as _, TestAppContext};
    use http_client::FakeHttpClient;
    use std::collections::HashMap;

    fn sources() -> Vec<CredentialSource> {
        vec![
            CredentialSource::Command {
                program: "gh".into(),
                args: vec!["auth".into(), "token".into()],
            },
            CredentialSource::EnvironmentVariable("FAKE_TOKEN".into()),
            CredentialSource::Keychain,
        ]
    }

    fn review(head_sha: &str) -> Review {
        Review {
            id: "PR_1".into(),
            number: 7,
            title: "Add review threads".into(),
            url: "https://fake.example/owner/repo/pull/7".parse().unwrap(),
            base_repository: ParsedGitRemote {
                owner: "owner".into(),
                repo: "repo".into(),
            },
            head_sha: head_sha.to_string().into(),
            base_ref: "main".into(),
            author_login: "author".into(),
        }
    }

    fn thread(id: &str, is_resolved: bool) -> ReviewThread {
        ReviewThread {
            id: ThreadId(id.to_string().into()),
            path: RepoPath::new("src/lib.rs").unwrap(),
            side: DiffSide::New,
            lines: Some(3..=4),
            original_lines: Some(3..=4),
            is_resolved,
            is_outdated: false,
            can_resolve: true,
            can_reply: true,
            resolved_by: None,
            comments: vec![ReviewComment {
                id: CommentId(format!("{id}-comment").into()),
                author: None,
                role: CommentAuthorRole::Reviewer,
                body: "Looks off".into(),
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                in_reply_to: None,
                url: None,
            }],
        }
    }

    fn target(provider: Arc<FakeCodeReviewProvider>, head_sha: &str) -> ReviewTarget {
        ReviewTarget {
            provider,
            query: ReviewQuery {
                base_repository: ParsedGitRemote {
                    owner: "owner".into(),
                    repo: "repo".into(),
                },
                head_owner: "owner".into(),
                head_branch: "feature".into(),
                base_branch: Some("main".into()),
            },
            local_head_sha: Some(head_sha.to_string().into()),
        }
    }

    fn store(
        cx: &mut TestAppContext,
        environment: FakeCredentialEnvironment,
        credentials: Arc<FakeCredentialsProvider>,
    ) -> gpui::Entity<ReviewStore> {
        cx.new(|_| {
            ReviewStore::with_dependencies(
                FakeHttpClient::with_404_response(),
                credentials,
                Arc::new(environment),
            )
        })
    }

    #[gpui::test]
    async fn test_token_sources_are_tried_in_order(cx: &mut TestAppContext) {
        let provider = FakeCodeReviewProvider {
            credential_sources: sources(),
            ..Default::default()
        };
        let credentials = FakeCredentialsProvider::default();
        credentials
            .state
            .lock()
            .keys
            .insert("https://fake.example".into(), b"keychain-token".to_vec());

        let environment = FakeCredentialEnvironment {
            variables: HashMap::from_iter([("FAKE_TOKEN".to_string(), " env-token\n".to_string())]),
            command_output: None,
        };
        let (token, attempts) =
            resolve_token(&provider, &credentials, &environment, &cx.to_async()).await;
        assert_eq!(token.as_deref(), Some("env-token"));
        assert_eq!(
            attempts
                .iter()
                .map(|attempt| attempt.outcome.clone())
                .collect::<Vec<_>>(),
            vec![
                CredentialOutcome::Failed("gh is not signed in".into()),
                CredentialOutcome::Found,
            ]
        );

        let environment = FakeCredentialEnvironment {
            variables: HashMap::default(),
            command_output: None,
        };
        let (token, attempts) =
            resolve_token(&provider, &credentials, &environment, &cx.to_async()).await;
        assert_eq!(token.as_deref(), Some("keychain-token"));
        assert_eq!(attempts.len(), 3);
    }

    #[gpui::test]
    async fn test_store_loads_review_and_threads(cx: &mut TestAppContext) {
        let provider = Arc::new(FakeCodeReviewProvider {
            credential_sources: sources(),
            ..Default::default()
        });
        {
            let mut state = provider.state.lock();
            state
                .reviews_by_branch
                .insert("feature".into(), review("abc123"));
            state.threads = vec![thread("open", false), thread("done", true)];
        }
        let store = store(
            cx,
            FakeCredentialEnvironment {
                variables: HashMap::default(),
                command_output: Some("cli-token".into()),
            },
            Arc::new(FakeCredentialsProvider::default()),
        );

        store.update(cx, |store, cx| {
            store.refresh(target(provider.clone(), "abc123"), cx);
            assert!(matches!(store.state(), ReviewState::Loading));
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let loaded = store.loaded().expect("review should be loaded");
            assert_eq!(loaded.review.number, 7);
            assert_eq!(loaded.threads.len(), 2);
            assert_eq!(loaded.open_thread_count(), 1);
            assert!(loaded.local_head_matches);
            assert!(!store.is_refreshing());
        });
        assert_eq!(provider.state.lock().tokens_seen, vec!["cli-token"]);

        store.update(cx, |store, cx| {
            store.refresh(target(provider.clone(), "different"), cx);
            assert!(
                store.loaded().is_some(),
                "threads stay visible while reloading the same review"
            );
        });
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert!(!store.loaded().unwrap().local_head_matches);
        });
    }

    #[gpui::test]
    async fn test_store_reports_signed_out_no_review_and_errors(cx: &mut TestAppContext) {
        let provider = Arc::new(FakeCodeReviewProvider {
            credential_sources: sources(),
            ..Default::default()
        });
        let credentials = Arc::new(FakeCredentialsProvider::default());
        let store = store(
            cx,
            FakeCredentialEnvironment {
                variables: HashMap::default(),
                command_output: None,
            },
            credentials.clone(),
        );

        store.update(cx, |store, cx| {
            store.refresh(target(provider.clone(), "abc"), cx)
        });
        cx.run_until_parked();
        store.read_with(cx, |store, _| match store.state() {
            ReviewState::SignedOut {
                provider_name,
                attempts,
            } => {
                assert_eq!(provider_name.as_ref(), "Fake");
                assert_eq!(attempts.len(), 3);
            }
            state => panic!("expected signed out, got {state:?}"),
        });

        store
            .update(cx, |store, cx| store.save_token("saved-token".into(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert!(
                matches!(store.state(), ReviewState::NoReview { head_branch, .. } if head_branch.as_ref() == "feature")
            );
        });
        assert_eq!(provider.state.lock().tokens_seen, vec!["saved-token"]);

        provider.state.lock().fail_with = Some("rate limited".into());
        store.update(cx, |store, cx| store.reload(cx));
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert!(
                matches!(store.state(), ReviewState::Failed(message) if message.contains("rate limited"))
            );
        });
    }

    #[test]
    fn test_review_target_for_fork_and_unsupported_hosts() {
        let registry = Arc::new(GitHostingProviderRegistry::new());
        registry
            .register_hosting_provider(Arc::new(git_hosting_providers::Github::public_instance()));
        registry
            .register_hosting_provider(Arc::new(git_hosting_providers::Gitlab::public_instance()));

        let fork = RepositoryReviewInfo {
            origin_url: Some("git@github.com:xenoop/zed.git".into()),
            upstream_url: Some("https://github.com/zed-industries/zed.git".into()),
            branch: Some("local-name".into()),
            remote_branch: Some("feat/review-threads".into()),
            head_sha: Some("abc".into()),
        };
        let target = review_target(&fork, "origin/main", registry.clone()).unwrap();
        assert_eq!(target.provider.name().as_ref(), "GitHub");
        assert_eq!(
            target.query,
            ReviewQuery {
                base_repository: ParsedGitRemote {
                    owner: "zed-industries".into(),
                    repo: "zed".into(),
                },
                head_owner: "xenoop".into(),
                head_branch: "feat/review-threads".into(),
                base_branch: Some("main".into()),
            }
        );

        let detached = RepositoryReviewInfo {
            branch: None,
            remote_branch: None,
            ..fork.clone()
        };
        assert_eq!(
            review_target(&detached, "main", registry.clone())
                .unwrap_err()
                .as_ref(),
            "Check out a branch to see its review threads."
        );

        let gitlab = RepositoryReviewInfo {
            origin_url: Some("https://gitlab.com/group/project.git".into()),
            upstream_url: None,
            ..fork
        };
        assert_eq!(
            review_target(&gitlab, "main", registry.clone())
                .unwrap_err()
                .as_ref(),
            "Review threads aren't supported for GitLab yet."
        );

        assert_eq!(
            review_target(&RepositoryReviewInfo::default(), "main", registry)
                .unwrap_err()
                .as_ref(),
            "This repository has no remote."
        );
    }

    #[test]
    fn test_branch_name_from_ref() {
        assert_eq!(branch_name_from_ref("main").as_deref(), Some("main"));
        assert_eq!(branch_name_from_ref("origin/main").as_deref(), Some("main"));
        assert_eq!(
            branch_name_from_ref("refs/remotes/upstream/release/1.0").as_deref(),
            Some("release/1.0")
        );
        assert_eq!(
            branch_name_from_ref("refs/heads/feat/x").as_deref(),
            Some("feat/x")
        );
        assert_eq!(branch_name_from_ref(""), None);
    }
}
