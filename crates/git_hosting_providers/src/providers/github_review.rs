use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use futures::AsyncReadExt as _;
use git::{
    ParsedGitRemote,
    code_review::{
        CodeReviewProvider, CommentAuthor, CommentAuthorRole, CommentId, CredentialSource,
        DiffSide, Review, ReviewCapabilities, ReviewComment, ReviewQuery, ReviewRequestContext,
        ReviewThread, ThreadId,
    },
    repository::RepoPath,
};
use gpui::SharedString;
use http_client::{AsyncBody, Method, Request};
use serde::Deserialize;
use serde_json::json;
use time::OffsetDateTime;
use url::Url;

/// Upper bound on review thread pages, so a huge pull request can't keep us
/// fetching forever.
const MAX_THREAD_PAGES: usize = 20;

/// Reads pull request review threads from github.com or a GitHub Enterprise
/// instance.
pub struct GithubReviews {
    name: SharedString,
    host: String,
    rest_api_url: String,
    graphql_url: String,
}

impl GithubReviews {
    pub fn new(name: impl Into<SharedString>, base_url: &Url) -> Self {
        let host = base_url.host_str().unwrap_or("github.com").to_string();
        let (rest_api_url, graphql_url) = if host == "github.com" {
            (
                "https://api.github.com".to_string(),
                "https://api.github.com/graphql".to_string(),
            )
        } else {
            (
                format!("https://{host}/api/v3"),
                format!("https://{host}/api/graphql"),
            )
        };
        Self {
            name: name.into(),
            host,
            rest_api_url,
            graphql_url,
        }
    }

    async fn send(
        &self,
        request: &ReviewRequestContext,
        method: Method,
        url: &str,
        body: Option<serde_json::Value>,
    ) -> Result<String> {
        let body = match body {
            Some(body) => AsyncBody::from(serde_json::to_string(&body)?),
            None => AsyncBody::default(),
        };
        let http_request = Request::builder()
            .method(method)
            .uri(url)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", format!("Bearer {}", request.token))
            .header("X-GitHub-Api-Version", "2022-11-28")
            .body(body)?;
        let mut response = request
            .http_client
            .send(http_request)
            .await
            .with_context(|| format!("requesting {url}"))?;
        let mut text = String::new();
        response.body_mut().read_to_string(&mut text).await?;

        let status = response.status();
        if status.as_u16() == 401 {
            bail!("{} rejected the token. Sign in again.", self.name);
        }
        if !status.is_success() {
            let message = serde_json::from_str::<ErrorBody>(&text)
                .map(|error| error.message)
                .unwrap_or(text);
            bail!("{} returned {}: {}", self.name, status.as_u16(), message);
        }
        Ok(text)
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    message: String,
}

#[derive(Deserialize)]
struct PullRequest {
    node_id: String,
    number: u64,
    title: String,
    html_url: Url,
    head: PullRequestRef,
    base: PullRequestRef,
    user: Option<PullRequestUser>,
}

#[derive(Deserialize)]
struct PullRequestRef {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
}

#[derive(Deserialize)]
struct PullRequestUser {
    login: String,
}

const REVIEW_THREADS_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 50, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          startLine
          originalLine
          originalStartLine
          diffSide
          viewerCanResolve
          viewerCanReply
          resolvedBy { login }
          comments(first: 100) {
            nodes {
              id
              author { login avatarUrl }
              body
              createdAt
              url
              replyTo { id }
            }
          }
        }
      }
    }
  }
}
"#;

#[derive(Deserialize)]
struct GraphQlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<ErrorBody>,
}

#[derive(Deserialize)]
struct ThreadsData {
    repository: Option<ThreadsRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadsRepository {
    pull_request: Option<ThreadsPullRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadsPullRequest {
    review_threads: Connection<ThreadNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    #[serde(default)]
    page_info: Option<PageInfo>,
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadNode {
    id: String,
    is_resolved: bool,
    is_outdated: bool,
    path: String,
    line: Option<u32>,
    start_line: Option<u32>,
    original_line: Option<u32>,
    original_start_line: Option<u32>,
    diff_side: String,
    viewer_can_resolve: bool,
    viewer_can_reply: bool,
    resolved_by: Option<LoginNode>,
    comments: Connection<CommentNode>,
}

#[derive(Deserialize)]
struct LoginNode {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthorNode {
    login: String,
    avatar_url: Option<Url>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentNode {
    id: String,
    author: Option<AuthorNode>,
    body: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    url: Option<Url>,
    reply_to: Option<IdNode>,
}

#[derive(Deserialize)]
struct IdNode {
    id: String,
}

fn line_range(start: Option<u32>, end: Option<u32>) -> Option<std::ops::RangeInclusive<u32>> {
    let end = end?;
    Some(start.filter(|start| *start <= end).unwrap_or(end)..=end)
}

fn convert_thread(node: ThreadNode, review_author: &str) -> Result<ReviewThread> {
    let comments = node
        .comments
        .nodes
        .into_iter()
        .map(|comment| {
            let role = match &comment.author {
                Some(author) if author.login == review_author => CommentAuthorRole::Author,
                _ => CommentAuthorRole::Reviewer,
            };
            ReviewComment {
                id: CommentId(comment.id.into()),
                author: comment.author.map(|author| CommentAuthor {
                    login: author.login.into(),
                    avatar_url: author.avatar_url,
                }),
                role,
                body: comment.body.into(),
                created_at: comment.created_at,
                in_reply_to: comment
                    .reply_to
                    .map(|reply_to| CommentId(reply_to.id.into())),
                url: comment.url,
            }
        })
        .collect();
    Ok(ReviewThread {
        id: ThreadId(node.id.into()),
        path: RepoPath::new(&node.path)
            .with_context(|| format!("invalid review thread path {:?}", node.path))?,
        side: if node.diff_side == "LEFT" {
            DiffSide::Old
        } else {
            DiffSide::New
        },
        lines: line_range(node.start_line, node.line),
        original_lines: line_range(node.original_start_line, node.original_line),
        is_resolved: node.is_resolved,
        is_outdated: node.is_outdated,
        can_resolve: node.viewer_can_resolve,
        can_reply: node.viewer_can_reply,
        resolved_by: node.resolved_by.map(|login| login.login.into()),
        comments,
    })
}

#[async_trait]
impl CodeReviewProvider for GithubReviews {
    fn name(&self) -> SharedString {
        self.name.clone()
    }

    fn capabilities(&self) -> ReviewCapabilities {
        ReviewCapabilities {
            nested_replies: false,
            resolve_threads: true,
            suggested_changes: true,
            verdicts: true,
        }
    }

    fn credential_key(&self) -> String {
        self.rest_api_url.clone()
    }

    fn credential_sources(&self) -> Vec<CredentialSource> {
        vec![
            CredentialSource::Command {
                program: "gh".into(),
                args: vec![
                    "auth".into(),
                    "token".into(),
                    "--hostname".into(),
                    self.host.clone().into(),
                ],
            },
            CredentialSource::EnvironmentVariable("GITHUB_TOKEN".into()),
            CredentialSource::EnvironmentVariable("GH_TOKEN".into()),
            CredentialSource::Keychain,
        ]
    }

    async fn find_review_for_branch(
        &self,
        request: &ReviewRequestContext,
        query: &ReviewQuery,
    ) -> Result<Option<Review>> {
        let ParsedGitRemote { owner, repo } = &query.base_repository;
        let mut url = Url::parse(&format!("{}/repos/{owner}/{repo}/pulls", self.rest_api_url))?;
        url.query_pairs_mut()
            .append_pair(
                "head",
                &format!("{}:{}", query.head_owner, query.head_branch),
            )
            .append_pair("state", "open")
            .append_pair("per_page", "20");
        let text = self.send(request, Method::GET, url.as_str(), None).await?;
        let pull_requests: Vec<PullRequest> =
            serde_json::from_str(&text).context("parsing pull requests")?;

        let preferred = pull_requests.iter().position(|pull_request| {
            query
                .base_branch
                .as_ref()
                .is_some_and(|base| *base == pull_request.base.ref_name)
        });
        let Some(pull_request) = pull_requests.into_iter().nth(preferred.unwrap_or_default())
        else {
            return Ok(None);
        };
        Ok(Some(Review {
            id: pull_request.node_id.into(),
            number: pull_request.number,
            title: pull_request.title.into(),
            url: pull_request.html_url,
            base_repository: query.base_repository.clone(),
            head_sha: pull_request.head.sha.into(),
            base_ref: pull_request.base.ref_name.into(),
            author_login: pull_request
                .user
                .map(|user| user.login)
                .unwrap_or_default()
                .into(),
        }))
    }

    async fn load_threads(
        &self,
        request: &ReviewRequestContext,
        review: &Review,
    ) -> Result<Vec<ReviewThread>> {
        let mut threads = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_THREAD_PAGES {
            let body = json!({
                "query": REVIEW_THREADS_QUERY,
                "variables": {
                    "owner": review.base_repository.owner.as_ref(),
                    "repo": review.base_repository.repo.as_ref(),
                    "number": review.number,
                    "cursor": cursor,
                },
            });
            let text = self
                .send(request, Method::POST, &self.graphql_url, Some(body))
                .await?;
            let response: GraphQlResponse<ThreadsData> =
                serde_json::from_str(&text).context("parsing review threads")?;
            if let Some(error) = response.errors.into_iter().next() {
                bail!("{} returned an error: {}", self.name, error.message);
            }
            let pull_request = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repository| repository.pull_request)
                .with_context(|| format!("pull request #{} not found", review.number))?;
            let connection = pull_request.review_threads;
            for node in connection.nodes {
                threads.push(convert_thread(node, &review.author_login)?);
            }
            match connection.page_info {
                Some(PageInfo {
                    has_next_page: true,
                    end_cursor: Some(end_cursor),
                }) => cursor = Some(end_cursor),
                _ => return Ok(threads),
            }
        }
        log::warn!(
            "stopped loading review threads for #{} after {MAX_THREAD_PAGES} pages",
            review.number
        );
        Ok(threads)
    }
}

pub(crate) fn github_reviews(name: &str, base_url: &Url) -> Arc<dyn CodeReviewProvider> {
    Arc::new(GithubReviews::new(name.to_string(), base_url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use http_client::{FakeHttpClient, Response};
    use parking_lot::Mutex;
    use pretty_assertions::assert_eq;

    fn context(responses: Vec<(u16, String)>) -> (ReviewRequestContext, Arc<Mutex<Vec<String>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(responses.into_iter()));
        let http_client = FakeHttpClient::create({
            let requests = requests.clone();
            move |request| {
                let auth = request
                    .headers()
                    .get("Authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                requests
                    .lock()
                    .push(format!("{} {} [{auth}]", request.method(), request.uri()));
                let (status, body) = responses.lock().next().expect("unexpected request");
                async move {
                    Ok(Response::builder()
                        .status(status)
                        .body(AsyncBody::from(body))
                        .unwrap())
                }
            }
        });
        (
            ReviewRequestContext {
                http_client,
                token: "secret".into(),
            },
            requests,
        )
    }

    fn query() -> ReviewQuery {
        ReviewQuery {
            base_repository: ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            head_owner: "xenoop".into(),
            head_branch: "feat/review-threads".into(),
            base_branch: Some("main".into()),
        }
    }

    fn pull_request_json(number: u64, base: &str) -> serde_json::Value {
        json!({
            "node_id": format!("PR_{number}"),
            "number": number,
            "title": "Add review threads",
            "html_url": format!("https://github.com/zed-industries/zed/pull/{number}"),
            "head": { "ref": "feat/review-threads", "sha": "abc123" },
            "base": { "ref": base, "sha": "def456" },
            "user": { "login": "xenoop" }
        })
    }

    #[test]
    fn test_find_review_prefers_matching_base_branch() {
        let reviews = GithubReviews::new("GitHub", &Url::parse("https://github.com").unwrap());
        let (request, requests) = context(vec![(
            200,
            json!([
                pull_request_json(1, "release"),
                pull_request_json(2, "main")
            ])
            .to_string(),
        )]);
        let review = block_on(reviews.find_review_for_branch(&request, &query()))
            .unwrap()
            .unwrap();
        assert_eq!(review.number, 2);
        assert_eq!(review.head_sha.as_ref(), "abc123");
        assert_eq!(review.author_login.as_ref(), "xenoop");
        assert_eq!(
            requests.lock().clone(),
            vec![
                "GET https://api.github.com/repos/zed-industries/zed/pulls?head=xenoop%3Afeat%2Freview-threads&state=open&per_page=20 [Bearer secret]"
                    .to_string()
            ]
        );
    }

    #[test]
    fn test_find_review_without_open_pull_request() {
        let reviews = GithubReviews::new("GitHub", &Url::parse("https://github.com").unwrap());
        let (request, _) = context(vec![(200, "[]".into())]);
        assert_eq!(
            block_on(reviews.find_review_for_branch(&request, &query())).unwrap(),
            None
        );
    }

    #[test]
    fn test_enterprise_urls_and_errors() {
        let reviews = GithubReviews::new(
            "GitHub Self-Hosted",
            &Url::parse("https://github.example.com").unwrap(),
        );
        assert_eq!(
            reviews.credential_key(),
            "https://github.example.com/api/v3"
        );
        assert_eq!(
            reviews.credential_sources()[0],
            CredentialSource::Command {
                program: "gh".into(),
                args: vec![
                    "auth".into(),
                    "token".into(),
                    "--hostname".into(),
                    "github.example.com".into()
                ],
            }
        );

        let (request, requests) = context(vec![(
            401,
            json!({"message": "Bad credentials"}).to_string(),
        )]);
        let error = block_on(reviews.find_review_for_branch(&request, &query())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "GitHub Self-Hosted rejected the token. Sign in again."
        );
        assert!(requests.lock()[0].starts_with("GET https://github.example.com/api/v3/repos/"));

        let (request, _) = context(vec![(
            403,
            json!({"message": "API rate limit exceeded"}).to_string(),
        )]);
        let error = block_on(reviews.find_review_for_branch(&request, &query())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "GitHub Self-Hosted returned 403: API rate limit exceeded"
        );
    }

    #[test]
    fn test_load_threads_paginates_and_converts() {
        let reviews = GithubReviews::new("GitHub", &Url::parse("https://github.com").unwrap());
        let thread = |id: &str, extra: serde_json::Value| {
            let mut thread = json!({
                "id": id,
                "isResolved": false,
                "isOutdated": false,
                "path": "crates/git_ui/src/branch_diff.rs",
                "line": 318,
                "startLine": 317,
                "originalLine": 318,
                "originalStartLine": 317,
                "diffSide": "RIGHT",
                "viewerCanResolve": true,
                "viewerCanReply": true,
                "resolvedBy": null,
                "comments": { "nodes": [
                    {
                        "id": format!("{id}-c1"),
                        "author": { "login": "rhea-l", "avatarUrl": "https://avatars.example/rhea" },
                        "body": "Should this stay visible?",
                        "createdAt": "2026-10-26T10:00:00Z",
                        "url": "https://github.com/zed-industries/zed/pull/2#discussion_r1",
                        "replyTo": null
                    },
                    {
                        "id": format!("{id}-c2"),
                        "author": { "login": "xenoop", "avatarUrl": null },
                        "body": "Good catch.",
                        "createdAt": "2026-10-26T11:00:00Z",
                        "url": null,
                        "replyTo": { "id": format!("{id}-c1") }
                    }
                ]}
            });
            if let (Some(thread), Some(extra)) = (thread.as_object_mut(), extra.as_object()) {
                for (key, value) in extra {
                    thread.insert(key.clone(), value.clone());
                }
            }
            thread
        };
        let first_page = json!({ "data": { "repository": { "pullRequest": { "reviewThreads": {
            "pageInfo": { "hasNextPage": true, "endCursor": "CURSOR" },
            "nodes": [thread("T1", json!({}))]
        }}}}});
        let second_page = json!({ "data": { "repository": { "pullRequest": { "reviewThreads": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": [thread("T2", json!({
                "isResolved": true,
                "isOutdated": true,
                "line": null,
                "startLine": null,
                "diffSide": "LEFT",
                "resolvedBy": { "login": "xenoop" }
            }))]
        }}}}});
        let (request, requests) = context(vec![
            (200, first_page.to_string()),
            (200, second_page.to_string()),
        ]);
        let review = Review {
            id: "PR_2".into(),
            number: 2,
            title: "".into(),
            url: Url::parse("https://github.com/zed-industries/zed/pull/2").unwrap(),
            base_repository: query().base_repository,
            head_sha: "abc123".into(),
            base_ref: "main".into(),
            author_login: "xenoop".into(),
        };
        let threads = block_on(reviews.load_threads(&request, &review)).unwrap();
        assert_eq!(requests.lock().len(), 2);
        assert!(requests.lock()[1].starts_with("POST https://api.github.com/graphql"));

        assert_eq!(threads.len(), 2);
        let first = &threads[0];
        assert_eq!(first.id.0.as_ref(), "T1");
        assert_eq!(first.lines, Some(317..=318));
        assert_eq!(first.side, DiffSide::New);
        assert_eq!(first.comments[0].role, CommentAuthorRole::Reviewer);
        assert_eq!(first.comments[1].role, CommentAuthorRole::Author);
        assert_eq!(
            first.comments[1].in_reply_to,
            Some(CommentId("T1-c1".into()))
        );
        assert_eq!(
            first.comments[0].created_at,
            time::macros::datetime!(2026-10-26 10:00 UTC)
        );

        let second = &threads[1];
        assert_eq!(second.lines, None);
        assert_eq!(second.original_lines, Some(317..=318));
        assert_eq!(second.side, DiffSide::Old);
        assert!(second.is_resolved && second.is_outdated);
        assert_eq!(second.resolved_by.as_deref(), Some("xenoop"));
    }

    #[test]
    fn test_graphql_errors_are_reported() {
        let reviews = GithubReviews::new("GitHub", &Url::parse("https://github.com").unwrap());
        let (request, _) = context(vec![(
            200,
            json!({ "data": null, "errors": [{ "message": "Resource not accessible by integration" }] })
                .to_string(),
        )]);
        let review = Review {
            id: "PR_2".into(),
            number: 2,
            title: "".into(),
            url: Url::parse("https://github.com/zed-industries/zed/pull/2").unwrap(),
            base_repository: query().base_repository,
            head_sha: "abc123".into(),
            base_ref: "main".into(),
            author_login: "xenoop".into(),
        };
        let error = block_on(reviews.load_threads(&request, &review)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "GitHub returned an error: Resource not accessible by integration"
        );
    }
}
