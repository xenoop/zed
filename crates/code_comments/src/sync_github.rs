//! Built-in GitHub sync provider, backed by the `gh` CLI so it reuses the
//! user's existing `gh` authentication and repository detection.

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

use crate::comment_sync::{
    CommentSyncProvider, OutgoingComment, RemoteComment, RepoContext, ReviewUnit,
};

pub struct GitHubSyncProvider;

/// One GitHub pull-request review comment as returned by the REST API.
#[derive(Deserialize)]
struct GhComment {
    id: i64,
    in_reply_to_id: Option<i64>,
    path: String,
    line: Option<u32>,
    original_line: Option<u32>,
    body: String,
    user: GhUser,
}

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

#[async_trait]
impl CommentSyncProvider for GitHubSyncProvider {
    fn name(&self) -> &str {
        "github"
    }

    async fn detect_review_unit(
        &self,
        ctx: &RepoContext,
        configured: Option<&str>,
    ) -> Result<Option<ReviewUnit>> {
        if let Some(id) = configured {
            return Ok(Some(ReviewUnit { id: id.to_string() }));
        }
        // `gh pr view` resolves the PR for the current branch.
        let number = run(
            &ctx.worktree_root,
            &["pr", "view", "--json", "number", "--jq", ".number"],
        )
        .await
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|out| !out.is_empty());
        Ok(number.map(|id| ReviewUnit { id }))
    }

    async fn fetch(&self, ctx: &RepoContext, unit: &ReviewUnit) -> Result<Vec<RemoteComment>> {
        let nwo = name_with_owner(&ctx.worktree_root).await?;
        let raw = run(
            &ctx.worktree_root,
            &[
                "api",
                "--paginate",
                &format!("repos/{nwo}/pulls/{}/comments", unit.id),
            ],
        )
        .await?;
        let comments: Vec<GhComment> =
            serde_json::from_str(&raw).context("parsing GitHub review comments")?;
        Ok(comments
            .into_iter()
            .map(|comment| RemoteComment {
                remote_id: comment.id.to_string(),
                parent_remote_id: comment.in_reply_to_id.map(|id| id.to_string()),
                path: comment.path,
                row: comment
                    .line
                    .or(comment.original_line)
                    .unwrap_or(1)
                    .saturating_sub(1),
                body: comment.body,
                author_login: comment.user.login,
            })
            .collect())
    }

    async fn push(
        &self,
        ctx: &RepoContext,
        unit: &ReviewUnit,
        comment: &OutgoingComment,
    ) -> Result<String> {
        let nwo = name_with_owner(&ctx.worktree_root).await?;
        let body_arg = format!("body={}", comment.body);

        let new_id = if let Some(parent) = &comment.parent_remote_id {
            run(
                &ctx.worktree_root,
                &[
                    "api",
                    "--method",
                    "POST",
                    &format!("repos/{nwo}/pulls/{}/comments/{parent}/replies", unit.id),
                    "-f",
                    &body_arg,
                    "--jq",
                    ".id",
                ],
            )
            .await?
        } else {
            // A new root review comment must be pinned to the PR head commit.
            let commit = run(
                &ctx.worktree_root,
                &[
                    "pr",
                    "view",
                    &unit.id,
                    "--json",
                    "headRefOid",
                    "--jq",
                    ".headRefOid",
                ],
            )
            .await?;
            run(
                &ctx.worktree_root,
                &[
                    "api",
                    "--method",
                    "POST",
                    &format!("repos/{nwo}/pulls/{}/comments", unit.id),
                    "-f",
                    &body_arg,
                    "-f",
                    &format!("commit_id={}", commit.trim()),
                    "-f",
                    &format!("path={}", comment.path),
                    "-F",
                    &format!("line={}", comment.row + 1),
                    "-f",
                    "side=RIGHT",
                    "--jq",
                    ".id",
                ],
            )
            .await?
        };

        Ok(new_id.trim().to_string())
    }
}

async fn name_with_owner(root: &Path) -> Result<String> {
    let nwo = run(
        root,
        &["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"],
    )
    .await?;
    Ok(nwo.trim().to_string())
}

/// Runs `gh` in the repository root and returns its stdout.
async fn run(root: &Path, args: &[&str]) -> Result<String> {
    let mut command = util::command::new_command("gh");
    command.args(args).current_dir(root);
    let output = command
        .output()
        .await
        .context("running the `gh` CLI (is it installed and authenticated?)")?;
    if !output.status.success() {
        bail!(
            "`gh {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
