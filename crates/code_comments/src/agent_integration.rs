//! Delivers comments to the active agent thread as structured feedback.
//!
//! - A **Question** thread is sent on demand; the agent's reply is appended to
//!   the thread as an `Agent`-authored child node.
//! - **Task** threads are delivered together by `SendTasksToAgent`.
//! - Plain **Comment** threads are never sent automatically.

use acp_thread::{AcpThread, AgentThreadEntry};
use agent_client_protocol::schema as acp;
use agent_ui::AgentPanel;
use gpui::{App, Entity};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{CommentAuthor, CommentKind, CommentNode, CommentStatus, CommentStore, CommentThread, ThreadId};

/// The agent thread currently focused in the workspace's agent panel.
fn active_thread(workspace: &Workspace, cx: &App) -> Option<Entity<AcpThread>> {
    workspace
        .panel::<AgentPanel>(cx)?
        .read(cx)
        .active_agent_thread(cx)
}

/// Renders one comment thread as structured feedback for the agent: file,
/// line range, the anchored line, and the comment tree.
fn build_feedback(thread: &CommentThread) -> String {
    let mut text = format!(
        "Inline code comment on `{}` (lines {}-{}):\n",
        thread.file.as_unix_str(),
        thread.anchor.start_row + 1,
        thread.anchor.end_row + 1,
    );
    if !thread.anchor.fingerprint.is_empty() {
        text.push_str(&format!("Anchored line: `{}`\n", thread.anchor.fingerprint));
    }
    text.push('\n');
    for node in &thread.nodes {
        let author = match &node.author {
            CommentAuthor::User => "User",
            CommentAuthor::Agent => "Agent",
            CommentAuthor::Remote { login } => login.as_str(),
        };
        text.push_str(&format!("- {author}: {}\n", node.body));
    }
    text
}

fn text_block(text: String) -> Vec<acp::ContentBlock> {
    vec![acp::ContentBlock::Text(acp::TextContent::new(text))]
}

/// Sends a single thread to the active agent thread and appends the agent's
/// reply to the comment tree as an `Agent`-authored node.
pub(crate) fn send_thread_to_agent(
    workspace: &Workspace,
    store: Entity<CommentStore>,
    thread_id: ThreadId,
    cx: &mut App,
) {
    let Some(acp_thread) = active_thread(workspace, cx) else {
        log::warn!("code_comments: no active agent thread to send the comment to");
        return;
    };
    let Some(thread) = store.read(cx).thread(thread_id).cloned() else {
        return;
    };

    let entries_before = acp_thread.read(cx).entries().len();
    let send = acp_thread.update(cx, |acp_thread, cx| {
        acp_thread.send(text_block(build_feedback(&thread)), cx)
    });

    cx.spawn(async move |cx| {
        if send.await.log_err().is_none() {
            return;
        }
        let answer = acp_thread.read_with(cx, |acp_thread, cx| {
            acp_thread
                .entries()
                .get(entries_before..)
                .unwrap_or(&[])
                .iter()
                .filter(|entry| matches!(entry, AgentThreadEntry::AssistantMessage(_)))
                .map(|entry| entry.to_markdown(cx))
                .collect::<Vec<_>>()
                .join("\n\n")
        });

        if !answer.trim().is_empty() {
            store.update(cx, |store, cx| {
                let parent = store
                    .thread(thread_id)
                    .and_then(|thread| thread.root().map(|root| root.id));
                store.add_node(
                    thread_id,
                    CommentNode::new(CommentAuthor::Agent, answer, parent, CommentKind::Comment),
                    cx,
                );
            });
        }
    })
    .detach();
}

/// Sends every open Task-kind thread to the agent as one batched message.
pub(crate) fn send_tasks_to_agent(
    workspace: &Workspace,
    store: Entity<CommentStore>,
    cx: &mut App,
) {
    let tasks: Vec<CommentThread> = store
        .read(cx)
        .all_threads()
        .filter(|thread| thread.kind == CommentKind::Task && thread.status == CommentStatus::Open)
        .cloned()
        .collect();
    if tasks.is_empty() {
        return;
    }
    let Some(acp_thread) = active_thread(workspace, cx) else {
        log::warn!("code_comments: no active agent thread to send tasks to");
        return;
    };

    let mut message = String::from("Please address the following review tasks:\n\n");
    for task in &tasks {
        message.push_str(&build_feedback(task));
        message.push('\n');
    }
    let send = acp_thread.update(cx, |acp_thread, cx| {
        acp_thread.send(text_block(message), cx)
    });
    cx.spawn(async move |_cx| {
        send.await.log_err();
    })
    .detach();
}
