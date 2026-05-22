//! The config-driven `custom` sync provider.
//!
//! It runs user-configured commands so a team can point comment sync at any
//! internal review system without writing Rust. The fetch command prints a
//! JSON array of [`RemoteComment`]s; the push command receives one
//! [`OutgoingComment`] as a JSON argument and prints the new remote id.

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;

use crate::comment_sync::{
    CommentSyncProvider, OutgoingComment, RemoteComment, RepoContext, ReviewUnit,
};

/// A sync provider whose operations are entirely defined by configured
/// commands. Each command is a program followed by its arguments.
pub struct CustomCommandSyncProvider {
    detect_command: Vec<String>,
    fetch_command: Vec<String>,
    push_command: Vec<String>,
}

impl CustomCommandSyncProvider {
    pub fn new(
        detect_command: Vec<String>,
        fetch_command: Vec<String>,
        push_command: Vec<String>,
    ) -> Self {
        Self {
            detect_command,
            fetch_command,
            push_command,
        }
    }
}

#[async_trait]
impl CommentSyncProvider for CustomCommandSyncProvider {
    fn name(&self) -> &str {
        "custom"
    }

    async fn detect_review_unit(
        &self,
        ctx: &RepoContext,
        configured: Option<&str>,
    ) -> Result<Option<ReviewUnit>> {
        if let Some(id) = configured {
            return Ok(Some(ReviewUnit { id: id.to_string() }));
        }
        if self.detect_command.is_empty() {
            return Ok(None);
        }
        let id = run(&ctx.worktree_root, &self.detect_command, None).await?;
        let id = id.trim().to_string();
        Ok((!id.is_empty()).then_some(ReviewUnit { id }))
    }

    async fn fetch(&self, ctx: &RepoContext, unit: &ReviewUnit) -> Result<Vec<RemoteComment>> {
        if self.fetch_command.is_empty() {
            bail!("code_comments: `custom` provider has no fetch_command configured");
        }
        let mut command = self.fetch_command.clone();
        command.push(unit.id.clone());
        let raw = run(&ctx.worktree_root, &command, None).await?;
        serde_json::from_str(&raw).context("parsing comments from the custom fetch command")
    }

    async fn push(
        &self,
        ctx: &RepoContext,
        unit: &ReviewUnit,
        comment: &OutgoingComment,
    ) -> Result<String> {
        if self.push_command.is_empty() {
            bail!("code_comments: `custom` provider has no push_command configured");
        }
        let payload = serde_json::to_string(comment).context("serializing the outgoing comment")?;
        let mut command = self.push_command.clone();
        command.push(unit.id.clone());
        let new_id = run(&ctx.worktree_root, &command, Some(&payload)).await?;
        Ok(new_id.trim().to_string())
    }
}

/// Runs a configured command (program + args) in the repository root,
/// optionally appending a JSON payload as the final argument.
async fn run(root: &Path, command: &[String], payload: Option<&str>) -> Result<String> {
    let Some((program, args)) = command.split_first() else {
        bail!("code_comments: empty custom sync command");
    };
    let mut process = util::command::new_command(program);
    process.args(args).current_dir(root);
    if let Some(payload) = payload {
        process.arg(payload);
    }
    let output = process
        .output()
        .await
        .with_context(|| format!("running custom sync command `{program}`"))?;
    if !output.status.success() {
        bail!(
            "custom sync command `{program}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
