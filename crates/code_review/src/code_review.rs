use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    future::Future,
    sync::Arc,
};

use anyhow::{Context as _, Result};
use credentials_provider::CredentialsProvider;
use futures::future::BoxFuture;
use git::{
    GitHostingProviderRegistry,
    code_review::{
        CodeReviewProvider, CommentAuthorRole, CredentialSource, DiffSide, DraftComment, Review,
        ReviewCapabilities, ReviewComment, ReviewQuery, ReviewRequestContext, ReviewSubmission,
        ReviewThread, ThreadId,
    },
    parse_git_remote_url,
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EntityId, EventEmitter, Global, SharedString,
    Task, WeakEntity,
};
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
    /// Describes the matching threads and their comments as Markdown, for an
    /// agent to read.
    pub fn describe_threads(&self, include: impl Fn(&ReviewThread) -> bool) -> String {
        let mut output = format!(
            "Pull request #{}: {}\n",
            self.review.number, self.review.title
        );
        if !self.local_head_matches {
            output.push_str(
                "Note: the local checkout is not at the pull request's latest commit, so line numbers may be off.\n",
            );
        }
        let threads = self
            .threads
            .iter()
            .filter(|thread| include(thread))
            .collect::<Vec<_>>();
        if threads.is_empty() {
            output.push_str("\nNo matching threads.\n");
            return output;
        }
        for thread in threads {
            let lines = thread
                .lines
                .clone()
                .or_else(|| thread.original_lines.clone())
                .map(|lines| {
                    if lines.start() == lines.end() {
                        format!(":{}", lines.end())
                    } else {
                        format!(":{}-{}", lines.start(), lines.end())
                    }
                })
                .unwrap_or_default();
            let mut status = Vec::new();
            if thread.is_resolved {
                status.push("resolved");
            }
            if thread.is_outdated {
                status.push("outdated");
            }
            if thread.side == DiffSide::Old {
                status.push("on removed lines");
            }
            let status = if status.is_empty() {
                String::new()
            } else {
                format!(" ({})", status.join(", "))
            };
            writeln!(
                output,
                "\n## Thread `{}` at {}{lines}{status}",
                thread.id.0,
                thread.path.as_unix_str()
            )
            .ok();
            for comment in &thread.comments {
                let author = comment
                    .author
                    .as_ref()
                    .map(|author| author.login.to_string())
                    .unwrap_or_else(|| "ghost".to_string());
                let role = match comment.role {
                    CommentAuthorRole::Author => " (pull request author)",
                    CommentAuthorRole::Reviewer => "",
                };
                writeln!(output, "- **{author}**{role}: {}", comment.body.trim()).ok();
            }
        }
        output
    }

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

enum ThreadUpdate {
    Replied(ReviewComment),
    Resolved(bool),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewStoreEvent {
    Changed,
}

/// A reply the agent wrote for a thread. Nothing is posted until the user
/// posts it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentReplyDraft {
    pub body: String,
    pub suggest_resolve: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindingSeverity {
    Bug,
    Suggestion,
    Nit,
    Question,
}

impl FindingSeverity {
    pub fn label(&self) -> &'static str {
        match self {
            FindingSeverity::Bug => "Bug",
            FindingSeverity::Suggestion => "Suggestion",
            FindingSeverity::Nit => "Nit",
            FindingSeverity::Question => "Question",
        }
    }
}

/// A comment the agent proposes for the review. The user keeps or dismisses
/// it; kept findings are posted with the review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFinding {
    pub id: usize,
    pub comment: DraftComment,
    pub severity: FindingSeverity,
    pub kept: bool,
}

#[derive(Default)]
struct GlobalReviewStores(HashMap<EntityId, WeakEntity<ReviewStore>>);

impl Global for GlobalReviewStores {}

/// Makes `store` the review store for a project, so other parts of Zed (like
/// agent tools) work on the same review the diff shows.
pub fn register_store_for_project(project_id: EntityId, store: &Entity<ReviewStore>, cx: &mut App) {
    let stores = &mut cx.default_global::<GlobalReviewStores>().0;
    stores.retain(|_, store| store.upgrade().is_some());
    stores.insert(project_id, store.downgrade());
}

pub fn store_for_project(project_id: EntityId, cx: &App) -> Option<Entity<ReviewStore>> {
    cx.try_global::<GlobalReviewStores>()?
        .0
        .get(&project_id)?
        .upgrade()
}

/// Holds the review threads for one repository's current branch.
pub struct ReviewStore {
    state: ReviewState,
    target: Option<ReviewTarget>,
    is_refreshing: bool,
    http_client: Arc<dyn HttpClient>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    environment: Arc<dyn CredentialEnvironment>,
    /// The authenticated context from the last successful load, reused for
    /// writes so they don't look the token up again.
    request: Option<ReviewRequestContext>,
    busy_threads: HashSet<ThreadId>,
    agent_replies: HashMap<ThreadId, AgentReplyDraft>,
    agent_working: HashSet<ThreadId>,
    findings: Vec<AgentFinding>,
    next_finding_id: usize,
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
            request: None,
            busy_threads: HashSet::default(),
            agent_replies: HashMap::default(),
            agent_working: HashSet::default(),
            findings: Vec::new(),
            next_finding_id: 0,
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

    pub fn agent_reply(&self, thread: &ThreadId) -> Option<&AgentReplyDraft> {
        self.agent_replies.get(thread)
    }

    pub fn is_agent_working(&self, thread: &ThreadId) -> bool {
        self.agent_working.contains(thread)
    }

    pub fn findings(&self) -> &[AgentFinding] {
        &self.findings
    }

    /// Marks threads as handed to the agent, until it drafts a reply for them.
    pub fn set_agent_working(
        &mut self,
        threads: impl IntoIterator<Item = ThreadId>,
        cx: &mut Context<Self>,
    ) {
        self.agent_working.extend(threads);
        self.changed(cx);
    }

    pub fn clear_agent_working(&mut self, cx: &mut Context<Self>) {
        self.agent_working.clear();
        self.changed(cx);
    }

    /// Stores the agent's draft reply for a thread. Fails if the thread isn't
    /// part of the loaded review.
    pub fn draft_agent_reply(
        &mut self,
        thread: ThreadId,
        draft: AgentReplyDraft,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let known = self.loaded().is_some_and(|loaded| {
            loaded
                .threads
                .iter()
                .any(|candidate| candidate.id == thread)
        });
        anyhow::ensure!(known, "no review thread with id {:?}", thread.0);
        self.agent_working.remove(&thread);
        self.agent_replies.insert(thread, draft);
        self.changed(cx);
        Ok(())
    }

    pub fn discard_agent_reply(&mut self, thread: &ThreadId, cx: &mut Context<Self>) {
        if self.agent_replies.remove(thread).is_some() {
            self.changed(cx);
        }
    }

    /// Posts the agent's draft reply, resolving the thread afterwards when asked.
    pub fn post_agent_reply(
        &mut self,
        thread: ThreadId,
        resolve: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(draft) = self.agent_replies.get(&thread).cloned() else {
            return Task::ready(Err(anyhow::anyhow!("no draft reply for this thread")));
        };
        let reply = self.reply(thread.clone(), draft.body, cx);
        cx.spawn(async move |this, cx| {
            reply.await?;
            this.update(cx, |this, cx| this.discard_agent_reply(&thread, cx))?;
            if resolve {
                this.update(cx, |this, cx| this.set_resolved(thread, true, cx))?
                    .await?;
            }
            Ok(())
        })
    }

    pub fn add_finding(
        &mut self,
        comment: DraftComment,
        severity: FindingSeverity,
        cx: &mut Context<Self>,
    ) -> usize {
        let id = self.next_finding_id;
        self.next_finding_id += 1;
        self.findings.push(AgentFinding {
            id,
            comment,
            severity,
            kept: false,
        });
        self.changed(cx);
        id
    }

    pub fn set_finding_kept(&mut self, id: usize, kept: bool, cx: &mut Context<Self>) {
        if let Some(finding) = self.findings.iter_mut().find(|finding| finding.id == id) {
            finding.kept = kept;
            self.changed(cx);
        }
    }

    pub fn dismiss_finding(&mut self, id: usize, cx: &mut Context<Self>) {
        self.findings.retain(|finding| finding.id != id);
        self.changed(cx);
    }

    /// The findings the user kept, to post with the review.
    pub fn kept_findings(&self) -> Vec<DraftComment> {
        self.findings
            .iter()
            .filter(|finding| finding.kept)
            .map(|finding| finding.comment.clone())
            .collect()
    }

    pub fn clear_kept_findings(&mut self, cx: &mut Context<Self>) {
        self.findings.retain(|finding| !finding.kept);
        self.changed(cx);
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(ReviewStoreEvent::Changed);
        cx.notify();
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
        if !same_target {
            self.agent_replies.clear();
            self.agent_working.clear();
            self.findings.clear();
        }
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
            let (state, request) =
                load_review_state(target, http_client, credentials_provider, environment, cx).await;
            this.update(cx, |this, cx| {
                this.is_refreshing = false;
                this.request = request;
                this.set_state(state, cx);
            })
            .log_err();
        }));
    }

    /// Reloads the last requested review, if any. Threads still marked as
    /// handed to the agent are released, since the agent may have skipped them.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.agent_working.clear();
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

    /// Whether a write to this thread is in flight.
    pub fn is_thread_busy(&self, thread: &ThreadId) -> bool {
        self.busy_threads.contains(thread)
    }

    /// Posts a reply at the end of a thread.
    pub fn reply(
        &mut self,
        thread: ThreadId,
        body: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.write_thread(
            thread.clone(),
            cx,
            move |provider, request, review| async move {
                let comment = provider.reply(&request, &review, &thread, &body).await?;
                Ok(ThreadUpdate::Replied(comment))
            },
        )
    }

    pub fn set_resolved(
        &mut self,
        thread: ThreadId,
        resolved: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.write_thread(thread.clone(), cx, move |provider, request, _| async move {
            provider.set_resolved(&request, &thread, resolved).await?;
            Ok(ThreadUpdate::Resolved(resolved))
        })
    }

    /// Submits a review with the given comments, then reloads the threads.
    pub fn submit_review(
        &mut self,
        submission: ReviewSubmission,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some((provider, request, review)) = self.write_context() else {
            return Task::ready(Err(anyhow::anyhow!("no review is loaded")));
        };
        let task = cx.background_spawn(async move {
            provider.submit_review(&request, &review, submission).await
        });
        cx.spawn(async move |this, cx| {
            task.await?;
            this.update(cx, |this, cx| this.reload(cx))?;
            Ok(())
        })
    }

    fn write_context(&self) -> Option<(Arc<dyn CodeReviewProvider>, ReviewRequestContext, Review)> {
        let provider = self.target.as_ref()?.provider.clone();
        let request = self.request.clone()?;
        let review = self.loaded()?.review.clone();
        Some((provider, request, review))
    }

    fn write_thread<F, Fut>(
        &mut self,
        thread: ThreadId,
        cx: &mut Context<Self>,
        write: F,
    ) -> Task<Result<()>>
    where
        F: FnOnce(Arc<dyn CodeReviewProvider>, ReviewRequestContext, Review) -> Fut,
        Fut: Future<Output = Result<ThreadUpdate>> + Send + 'static,
    {
        let Some((provider, request, review)) = self.write_context() else {
            return Task::ready(Err(anyhow::anyhow!("no review is loaded")));
        };
        if !self.busy_threads.insert(thread.clone()) {
            return Task::ready(Ok(()));
        }
        cx.emit(ReviewStoreEvent::Changed);
        cx.notify();

        let task = cx.background_spawn(write(provider, request, review));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.busy_threads.remove(&thread);
                if result.is_ok() {
                    this.agent_working.remove(&thread);
                }
                if let Ok(update) = &result
                    && let ReviewState::Loaded(loaded) = &mut this.state
                    && let Some(loaded_thread) = loaded
                        .threads
                        .iter_mut()
                        .find(|candidate| candidate.id == thread)
                {
                    match update {
                        ThreadUpdate::Replied(comment) => {
                            loaded_thread.comments.push(comment.clone())
                        }
                        ThreadUpdate::Resolved(resolved) => loaded_thread.is_resolved = *resolved,
                    }
                }
                cx.emit(ReviewStoreEvent::Changed);
                cx.notify();
            })?;
            result.map(|_| ())
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
) -> (ReviewState, Option<ReviewRequestContext>) {
    let provider = target.provider.clone();
    let (token, attempts) = resolve_token(
        provider.as_ref(),
        credentials_provider.as_ref(),
        environment.as_ref(),
        cx,
    )
    .await;
    let Some(token) = token else {
        return (
            ReviewState::SignedOut {
                provider_name: provider.name(),
                attempts,
            },
            None,
        );
    };

    let request = ReviewRequestContext { http_client, token };
    let query = target.query.clone();
    let result = cx
        .background_executor()
        .spawn({
            let request = request.clone();
            async move {
                let Some(review) = provider.find_review_for_branch(&request, &query).await? else {
                    return anyhow::Ok(None);
                };
                let threads = provider.load_threads(&request, &review).await?;
                Ok(Some((review, threads, provider.capabilities())))
            }
        })
        .await;

    let state = match result {
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
    };
    (state, Some(request))
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
        pub replies: Vec<(ThreadId, String)>,
        pub submissions: Vec<ReviewSubmission>,
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

        async fn reply(
            &self,
            _request: &ReviewRequestContext,
            _review: &Review,
            thread: &ThreadId,
            body: &str,
        ) -> Result<ReviewComment> {
            let mut state = self.state.lock();
            if let Some(error) = &state.fail_with {
                anyhow::bail!("{error}");
            }
            state.replies.push((thread.clone(), body.to_string()));
            let comment = ReviewComment {
                id: git::code_review::CommentId(format!("reply-{}", state.replies.len()).into()),
                author: None,
                role: git::code_review::CommentAuthorRole::Author,
                body: body.to_string().into(),
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                in_reply_to: None,
                url: None,
            };
            if let Some(thread) = state
                .threads
                .iter_mut()
                .find(|candidate| candidate.id == *thread)
            {
                thread.comments.push(comment.clone());
            }
            Ok(comment)
        }

        async fn set_resolved(
            &self,
            _request: &ReviewRequestContext,
            thread: &ThreadId,
            resolved: bool,
        ) -> Result<()> {
            let mut state = self.state.lock();
            if let Some(error) = &state.fail_with {
                anyhow::bail!("{error}");
            }
            if let Some(thread) = state
                .threads
                .iter_mut()
                .find(|candidate| candidate.id == *thread)
            {
                thread.is_resolved = resolved;
            }
            Ok(())
        }

        async fn submit_review(
            &self,
            _request: &ReviewRequestContext,
            _review: &Review,
            submission: ReviewSubmission,
        ) -> Result<()> {
            let mut state = self.state.lock();
            if let Some(error) = &state.fail_with {
                anyhow::bail!("{error}");
            }
            state.submissions.push(submission);
            Ok(())
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
    use gpui::TestAppContext;
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
            viewer_login: Some("author".into()),
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
    async fn test_store_writes(cx: &mut TestAppContext) {
        let provider = Arc::new(FakeCodeReviewProvider {
            credential_sources: sources(),
            ..Default::default()
        });
        {
            let mut state = provider.state.lock();
            state
                .reviews_by_branch
                .insert("feature".into(), review("abc123"));
            state.threads = vec![thread("open", false)];
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
            store.refresh(target(provider.clone(), "abc123"), cx)
        });
        cx.run_until_parked();

        let thread_id = ThreadId("open".into());
        let reply = store.update(cx, |store, cx| {
            let task = store.reply(thread_id.clone(), "Fixed it".into(), cx);
            assert!(store.is_thread_busy(&thread_id));
            task
        });
        reply.await.unwrap();
        store.read_with(cx, |store, _| {
            let thread = &store.loaded().unwrap().threads[0];
            assert_eq!(thread.comments.len(), 2);
            assert_eq!(thread.comments[1].body.as_ref(), "Fixed it");
            assert!(!store.is_thread_busy(&thread_id));
        });

        store
            .update(cx, |store, cx| {
                store.set_resolved(thread_id.clone(), true, cx)
            })
            .await
            .unwrap();
        store.read_with(cx, |store, _| {
            assert!(store.loaded().unwrap().threads[0].is_resolved);
        });

        let submission = ReviewSubmission {
            verdict: git::code_review::ReviewVerdict::Comment,
            body: "Thanks".into(),
            comments: Vec::new(),
        };
        store
            .update(cx, |store, cx| store.submit_review(submission.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(provider.state.lock().submissions, vec![submission]);
        assert_eq!(
            provider.state.lock().tokens_seen,
            vec!["cli-token", "cli-token"],
            "writes reuse the token, and submitting reloads once"
        );

        provider.state.lock().fail_with = Some("forbidden".into());
        let error = store
            .update(cx, |store, cx| {
                store.reply(thread_id.clone(), "Again".into(), cx)
            })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "forbidden");
        store.read_with(cx, |store, _| {
            assert_eq!(store.loaded().unwrap().threads[0].comments.len(), 2);
        });
    }

    #[gpui::test]
    async fn test_agent_drafts_and_findings(cx: &mut TestAppContext) {
        let provider = Arc::new(FakeCodeReviewProvider {
            credential_sources: sources(),
            ..Default::default()
        });
        {
            let mut state = provider.state.lock();
            state
                .reviews_by_branch
                .insert("feature".into(), review("abc123"));
            state.threads = vec![thread("open", false)];
        }
        let store = store(
            cx,
            FakeCredentialEnvironment {
                variables: HashMap::default(),
                command_output: Some("token".into()),
            },
            Arc::new(FakeCredentialsProvider::default()),
        );
        store.update(cx, |store, cx| {
            store.refresh(target(provider.clone(), "abc123"), cx)
        });
        cx.run_until_parked();

        let thread_id = ThreadId("open".into());
        store.update(cx, |store, cx| {
            store.set_agent_working([thread_id.clone()], cx);
            assert!(store.is_agent_working(&thread_id));
            assert!(
                store
                    .draft_agent_reply(
                        ThreadId("unknown".into()),
                        AgentReplyDraft {
                            body: "x".into(),
                            suggest_resolve: false
                        },
                        cx
                    )
                    .is_err()
            );
            store
                .draft_agent_reply(
                    thread_id.clone(),
                    AgentReplyDraft {
                        body: "Fixed in the latest commit.".into(),
                        suggest_resolve: true,
                    },
                    cx,
                )
                .unwrap();
            assert!(!store.is_agent_working(&thread_id));
        });

        store
            .update(cx, |store, cx| {
                store.post_agent_reply(thread_id.clone(), true, cx)
            })
            .await
            .unwrap();
        assert_eq!(
            provider.state.lock().replies,
            vec![(thread_id.clone(), "Fixed in the latest commit.".to_string())]
        );
        store.read_with(cx, |store, _| {
            assert!(store.agent_reply(&thread_id).is_none());
            assert!(store.loaded().unwrap().threads[0].is_resolved);
        });

        store.update(cx, |store, cx| {
            let comment = |line| git::code_review::DraftComment {
                path: RepoPath::new("src/lib.rs").unwrap(),
                side: DiffSide::New,
                lines: line..=line,
                body: format!("finding at {line}"),
            };
            let kept = store.add_finding(comment(3), FindingSeverity::Bug, cx);
            let dismissed = store.add_finding(comment(9), FindingSeverity::Nit, cx);
            store.set_finding_kept(kept, true, cx);
            store.dismiss_finding(dismissed, cx);
            assert_eq!(store.findings().len(), 1);
            assert_eq!(store.kept_findings(), vec![comment(3)]);
            store.clear_kept_findings(cx);
            assert!(store.findings().is_empty());
        });
    }

    #[test]
    fn test_describe_threads() {
        let mut open = thread("open", false);
        open.comments.push(ReviewComment {
            id: CommentId("reply".into()),
            author: Some(git::code_review::CommentAuthor {
                login: "author".into(),
                avatar_url: None,
            }),
            role: CommentAuthorRole::Author,
            body: "Will do.".into(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            in_reply_to: None,
            url: None,
        });
        let loaded = LoadedReview {
            review: review("abc"),
            threads: vec![open, thread("done", true)],
            capabilities: ReviewCapabilities::default(),
            local_head_matches: false,
        };
        let text = loaded.describe_threads(|thread| !thread.is_resolved);
        assert_eq!(
            text,
            "Pull request #7: Add review threads\n\
             Note: the local checkout is not at the pull request's latest commit, so line numbers may be off.\n\
             \n## Thread `open` at src/lib.rs:3-4\n\
             - **ghost**: Looks off\n\
             - **author** (pull request author): Will do.\n"
        );
        assert!(
            loaded
                .describe_threads(|_| true)
                .contains("Thread `done` at src/lib.rs:3-4 (resolved)")
        );
    }

    #[gpui::test]
    fn test_store_registry(cx: &mut TestAppContext) {
        let project = cx.new(|_| ());
        let store = store(
            cx,
            FakeCredentialEnvironment {
                variables: HashMap::default(),
                command_output: None,
            },
            Arc::new(FakeCredentialsProvider::default()),
        );
        cx.update(|cx| {
            assert!(store_for_project(project.entity_id(), cx).is_none());
            register_store_for_project(project.entity_id(), &store, cx);
            assert_eq!(
                store_for_project(project.entity_id(), cx).map(|store| store.entity_id()),
                Some(store.entity_id())
            );
        });
        drop(store);
        cx.run_until_parked();
        cx.update(|cx| assert!(store_for_project(project.entity_id(), cx).is_none()));
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
