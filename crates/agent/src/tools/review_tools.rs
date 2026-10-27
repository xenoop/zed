use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v2 as acp;
use code_review::{AgentReplyDraft, FindingSeverity, ReviewStore, store_for_project};
use git::{
    code_review::{DiffSide, DraftComment, ThreadId},
    repository::RepoPath,
};
use gpui::{App, AsyncApp, Entity, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use ui::SharedString;

const NO_REVIEW: &str = "No pull request review is open. Ask the user to open the branch diff and turn on Review Threads.";

fn review_store(
    project: &Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<Entity<ReviewStore>, String> {
    cx.update(|cx| store_for_project(project.entity_id(), cx))
        .filter(|store| cx.update(|cx| store.read(cx).loaded().is_some()))
        .ok_or_else(|| NO_REVIEW.to_string())
}

/// Lists the review threads on the current branch's pull request, with every comment.
///
/// Use this before addressing review feedback, and before proposing review comments so you don't repeat what reviewers already said.
/// Each thread has an `id` to pass to `draft_review_reply`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReviewThreadsToolInput {
    /// Include threads that were already resolved.
    #[serde(default)]
    pub include_resolved: bool,
}

pub struct ReviewThreadsTool {
    project: Entity<Project>,
}

impl ReviewThreadsTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for ReviewThreadsTool {
    type Input = ReviewThreadsToolInput;
    type Output = String;

    const NAME: &'static str = "review_threads";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Read review threads".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            let store = review_store(&project, cx)?;
            Ok(cx.update(|cx| {
                let store = store.read(cx);
                let Some(loaded) = store.loaded() else {
                    return NO_REVIEW.to_string();
                };
                loaded.describe_threads(|thread| input.include_resolved || !thread.is_resolved)
            }))
        })
    }
}

/// Drafts a reply to a pull request review thread. The draft is shown to the user next to the thread; it is never posted until the user posts it.
///
/// Write one reply per thread you addressed: say briefly what changed, or why no change was made.
/// Set `suggest_resolve` when the feedback is fully addressed.
/// Don't draft replies for threads you didn't act on, such as design questions only the user can answer.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DraftReviewReplyToolInput {
    /// The thread's `id`, as listed by `review_threads`.
    pub thread_id: String,
    /// The reply, in Markdown.
    pub body: String,
    /// Whether the thread can be resolved once the reply is posted.
    #[serde(default)]
    pub suggest_resolve: bool,
}

pub struct DraftReviewReplyTool {
    project: Entity<Project>,
}

impl DraftReviewReplyTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for DraftReviewReplyTool {
    type Input = DraftReviewReplyToolInput;
    type Output = String;

    const NAME: &'static str = "draft_review_reply";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Draft review reply".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            if input.body.trim().is_empty() {
                return Err("The reply is empty.".to_string());
            }
            let store = review_store(&project, cx)?;
            store
                .update(cx, |store, cx| {
                    store.draft_agent_reply(
                        ThreadId(input.thread_id.into()),
                        AgentReplyDraft {
                            body: input.body,
                            suggest_resolve: input.suggest_resolve,
                        },
                        cx,
                    )
                })
                .map_err(|error| error.to_string())?;
            Ok("Draft saved. The user will review it and decide whether to post it.".to_string())
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCommentSeverity {
    Bug,
    Suggestion,
    Nit,
    Question,
}

/// Proposes a line comment for the user's pull request review. The user keeps or dismisses each proposal; kept ones are posted under the user's name when they finish the review.
///
/// Use this when reviewing a branch diff, once per finding, on the lines the finding is about.
/// Check `review_threads` first and don't repeat points reviewers already raised.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DraftReviewCommentToolInput {
    /// The file the comment is on. Either a project path (starting with a root directory of the project) or a path relative to the repository root.
    pub path: String,
    /// The first line the comment covers (1-based, in the current version of the file).
    pub start_line: u32,
    /// The last line the comment covers (1-based, inclusive).
    pub end_line: u32,
    /// The comment, in Markdown.
    pub body: String,
    pub severity: ReviewCommentSeverity,
}

pub struct DraftReviewCommentTool {
    project: Entity<Project>,
}

impl DraftReviewCommentTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

fn repo_path(project: &Entity<Project>, path: &str, cx: &App) -> Option<RepoPath> {
    let project = project.read(cx);
    if let Some(project_path) = project.find_project_path(path, cx)
        && let Some((_, repo_path)) = project
            .git_store()
            .read(cx)
            .repository_and_path_for_project_path(&project_path, cx)
    {
        return Some(repo_path);
    }
    RepoPath::new(path).ok()
}

impl AgentTool for DraftReviewCommentTool {
    type Input = DraftReviewCommentToolInput;
    type Output = String;

    const NAME: &'static str = "draft_review_comment";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Propose review comment on {}", input.path).into(),
            Err(_) => "Propose review comment".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            if input.body.trim().is_empty() {
                return Err("The comment is empty.".to_string());
            }
            if input.start_line == 0 || input.end_line < input.start_line {
                return Err(
                    "Lines are 1-based and end_line must not be before start_line.".to_string(),
                );
            }
            let store = review_store(&project, cx)?;
            let path = cx
                .update(|cx| repo_path(&project, &input.path, cx))
                .ok_or_else(|| format!("{} is not a valid path in this repository", input.path))?;
            let severity = match input.severity {
                ReviewCommentSeverity::Bug => FindingSeverity::Bug,
                ReviewCommentSeverity::Suggestion => FindingSeverity::Suggestion,
                ReviewCommentSeverity::Nit => FindingSeverity::Nit,
                ReviewCommentSeverity::Question => FindingSeverity::Question,
            };
            store.update(cx, |store, cx| {
                store.add_finding(
                    DraftComment {
                        path,
                        side: DiffSide::New,
                        lines: input.start_line..=input.end_line,
                        body: input.body,
                    },
                    severity,
                    cx,
                )
            });
            Ok("Comment proposed. The user will keep or dismiss it.".to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_review::{
        ReviewTarget,
        fake::{FakeCodeReviewProvider, FakeCredentialEnvironment},
        register_store_for_project,
    };
    use credentials_provider::test_support::FakeCredentialsProvider;
    use git::{
        ParsedGitRemote,
        code_review::{
            CommentAuthorRole, CommentId, CredentialSource, Review, ReviewComment, ReviewQuery,
            ReviewThread,
        },
    };
    use gpui::{AppContext as _, TestAppContext};
    use project::FakeFs;
    use settings::SettingsStore;
    use std::collections::HashMap;
    use util::path;

    async fn setup(cx: &mut TestAppContext) -> (Entity<Project>, Entity<ReviewStore>) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            serde_json::json!({ "src": { "lib.rs": "fn main() {}\n" } }),
        )
        .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

        let provider = Arc::new(FakeCodeReviewProvider {
            credential_sources: vec![CredentialSource::EnvironmentVariable("TOKEN".into())],
            ..Default::default()
        });
        let base_repository = ParsedGitRemote {
            owner: "owner".into(),
            repo: "repo".into(),
        };
        {
            let mut state = provider.state.lock();
            state.reviews_by_branch.insert(
                "feature".into(),
                Review {
                    id: "PR_1".into(),
                    number: 1,
                    title: "Tidy".into(),
                    url: "https://fake.example/pull/1".parse().unwrap(),
                    base_repository: base_repository.clone(),
                    head_sha: "sha".into(),
                    base_ref: "main".into(),
                    author_login: "author".into(),
                    viewer_login: None,
                },
            );
            state.threads = vec![ReviewThread {
                id: ThreadId("T1".into()),
                path: RepoPath::new("src/lib.rs").unwrap(),
                side: DiffSide::New,
                lines: Some(1..=1),
                original_lines: Some(1..=1),
                is_resolved: false,
                is_outdated: false,
                can_resolve: true,
                can_reply: true,
                resolved_by: None,
                comments: vec![ReviewComment {
                    id: CommentId("C1".into()),
                    author: None,
                    role: CommentAuthorRole::Reviewer,
                    body: "Rename main".into(),
                    created_at: time::OffsetDateTime::UNIX_EPOCH,
                    in_reply_to: None,
                    url: None,
                }],
            }];
        }
        let store = cx.new(|_| {
            ReviewStore::with_dependencies(
                http_client::FakeHttpClient::with_404_response(),
                Arc::new(FakeCredentialsProvider::default()),
                Arc::new(FakeCredentialEnvironment {
                    variables: HashMap::from_iter([("TOKEN".to_string(), "t".to_string())]),
                    command_output: None,
                }),
            )
        });
        store.update(cx, |store, cx| {
            store.refresh(
                ReviewTarget {
                    provider,
                    query: ReviewQuery {
                        base_repository,
                        head_owner: "owner".into(),
                        head_branch: "feature".into(),
                        base_branch: Some("main".into()),
                    },
                    local_head_sha: Some("sha".into()),
                },
                cx,
            )
        });
        cx.run_until_parked();
        (project, store)
    }

    #[gpui::test]
    async fn test_review_tools_without_review(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let tool = Arc::new(ReviewThreadsTool::new(project));
        let (event_stream, _) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(ReviewThreadsToolInput {
                        include_resolved: false,
                    }),
                    event_stream,
                    cx,
                )
            })
            .await;
        assert_eq!(result, Err(NO_REVIEW.to_string()));
    }

    #[gpui::test]
    async fn test_review_tools_read_and_draft(cx: &mut TestAppContext) {
        let (project, store) = setup(cx).await;
        cx.update(|cx| register_store_for_project(project.entity_id(), &store, cx));

        let (event_stream, _) = ToolCallEventStream::test();
        let threads = cx
            .update(|cx| {
                Arc::new(ReviewThreadsTool::new(project.clone())).run(
                    ToolInput::resolved(ReviewThreadsToolInput {
                        include_resolved: false,
                    }),
                    event_stream,
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(
            threads.contains("## Thread `T1` at src/lib.rs:1"),
            "{threads}"
        );
        assert!(threads.contains("Rename main"));

        let (event_stream, _) = ToolCallEventStream::test();
        cx.update(|cx| {
            Arc::new(DraftReviewReplyTool::new(project.clone())).run(
                ToolInput::resolved(DraftReviewReplyToolInput {
                    thread_id: "T1".into(),
                    body: "Renamed to `run`.".into(),
                    suggest_resolve: true,
                }),
                event_stream,
                cx,
            )
        })
        .await
        .unwrap();

        let (event_stream, _) = ToolCallEventStream::test();
        let error = cx
            .update(|cx| {
                Arc::new(DraftReviewCommentTool::new(project.clone())).run(
                    ToolInput::resolved(DraftReviewCommentToolInput {
                        path: "root/src/lib.rs".into(),
                        start_line: 3,
                        end_line: 1,
                        body: "x".into(),
                        severity: ReviewCommentSeverity::Nit,
                    }),
                    event_stream,
                    cx,
                )
            })
            .await
            .unwrap_err();
        assert!(error.contains("end_line"));

        let (event_stream, _) = ToolCallEventStream::test();
        cx.update(|cx| {
            Arc::new(DraftReviewCommentTool::new(project.clone())).run(
                ToolInput::resolved(DraftReviewCommentToolInput {
                    path: "src/lib.rs".into(),
                    start_line: 1,
                    end_line: 1,
                    body: "This never returns.".into(),
                    severity: ReviewCommentSeverity::Bug,
                }),
                event_stream,
                cx,
            )
        })
        .await
        .unwrap();

        store.read_with(cx, |store, _| {
            let draft = store.agent_reply(&ThreadId("T1".into())).unwrap();
            assert_eq!(draft.body, "Renamed to `run`.");
            assert!(draft.suggest_resolve);
            let findings = store.findings();
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].comment.path.as_unix_str(), "src/lib.rs");
            assert_eq!(findings[0].severity, FindingSeverity::Bug);
            assert!(!findings[0].kept);
        });
    }
}
