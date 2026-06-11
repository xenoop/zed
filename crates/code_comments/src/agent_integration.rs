//! Delivers comments to the active agent thread as structured feedback.
//!
//! - A **Question** thread is sent on demand; the agent's reply is appended to
//!   the thread as an `Agent`-authored child node.
//! - **Task** threads are delivered together by `SendTasksToAgent`.
//! - Plain **Comment** threads are never sent automatically.

use acp_thread::{AcpThread, AgentThreadEntry, AssistantMessageChunk};
use agent_client_protocol::schema as acp;
use agent_ui::AgentPanel;
use gpui::{App, Entity};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{
    CommentAuthor, CommentId, CommentKind, Comment, ConversationStatus, CommentStore,
    Conversation, ConversationId,
};

/// The agent thread currently focused in the workspace's agent panel.
fn active_thread(workspace: &Workspace, cx: &App) -> Option<Entity<AcpThread>> {
    workspace
        .panel::<AgentPanel>(cx)?
        .read(cx)
        .active_agent_thread(cx)
}

/// Renders one comment thread as structured feedback for the agent: file,
/// line range, the anchored line, and the comment tree.
fn build_feedback(thread: &Conversation) -> String {
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

/// Sends a single comment node to the agent. The prompt is scoped to that
/// node and its ancestor chain (so the agent sees the conversation leading up
/// to the comment, not unrelated siblings). The agent's reply is appended as
/// a child of the source node.
pub(crate) fn send_comment_to_agent(
    workspace: &Workspace,
    store: Entity<CommentStore>,
    conversation_id: ConversationId,
    node_id: CommentId,
    cx: &mut App,
) {
    let Some(acp_thread) = active_thread(workspace, cx) else {
        log::warn!("code_comments: no active agent thread to send the comment to");
        return;
    };
    let Some(thread) = store.read(cx).thread(conversation_id).cloned() else {
        return;
    };
    let Some(prompt) = build_node_feedback(&thread, node_id) else {
        log::warn!("code_comments: send_comment_to_agent could not find node {node_id:?}");
        return;
    };

    let entries_before = acp_thread.read(cx).entries().len();
    let send = acp_thread.update(cx, |acp_thread, cx| {
        acp_thread.send(text_block(prompt), cx)
    });

    cx.spawn(async move |cx| {
        if send.await.log_err().is_none() {
            return;
        }
        let answer = acp_thread.read_with(cx, |acp_thread, cx| {
            assistant_reply_markdown(acp_thread, entries_before, cx)
        });

        if !answer.trim().is_empty() {
            store.update(cx, |store, cx| {
                store.add_comment(
                    conversation_id,
                    Comment::new(
                        CommentAuthor::Agent,
                        answer,
                        Some(node_id),
                        CommentKind::Comment,
                    ),
                    cx,
                );
            });
        }
    })
    .detach();
}

/// Collects the assistant's reply as raw markdown, skipping the
/// `## Assistant` wrapper and any `<thinking>` chunks. Returns the joined
/// message body suitable for storing as a comment node.
fn assistant_reply_markdown(acp_thread: &AcpThread, entries_before: usize, cx: &App) -> String {
    let mut pieces = Vec::new();
    for entry in acp_thread.entries().iter().skip(entries_before) {
        if let AgentThreadEntry::AssistantMessage(message) = entry {
            for chunk in &message.chunks {
                if let AssistantMessageChunk::Message { block } = chunk {
                    let text = block.to_markdown(cx);
                    if !text.is_empty() {
                        pieces.push(text.to_string());
                    }
                }
            }
        }
    }
    pieces.join("\n\n").trim().to_string()
}

/// Builds the prompt for `send_comment_to_agent`.
///
/// Layout:
/// 1. The target comment's body verbatim — the actual question/task the agent
///    should respond to. This leads so the panel doesn't open on a wall of
///    context.
/// 2. A `---` separator.
/// 3. A `**Context**` block with the worktree-relative file path + compact
///    line range, the anchored line, and a fenced-code thread dump where:
///    - depth-indented bullets mirror the reply tree;
///    - `►► ` marks the target node;
///    - parents that already have an `Agent` reply get a `(replied)` suffix;
///    - the whole thread is suffixed with `(resolved)` when applicable.
fn build_node_feedback(thread: &Conversation, target: CommentId) -> Option<String> {
    let target_node = thread.nodes.iter().find(|node| node.id == target)?;
    let root = thread.root()?;

    let resolved_suffix = if thread.status == ConversationStatus::Resolved {
        " (resolved)"
    } else {
        ""
    };

    let mut tree = String::new();
    append_subtree(&mut tree, thread, root, 0, target);

    let mut out = String::new();
    out.push_str(target_node.body.trim());
    out.push_str("\n\n---\n\n");
    out.push_str(&format!(
        "**Context** — comment on `{}{}`{}\n",
        thread.file.as_unix_str(),
        format_line_range(thread),
        resolved_suffix,
    ));
    if !thread.anchor.fingerprint.is_empty() {
        out.push_str(&format!("`{}`\n", thread.anchor.fingerprint));
    }
    out.push_str("\nThread so far:\n\n```\n");
    out.push_str(&tree);
    out.push_str("```\n");
    Some(out)
}

/// `:N` for a single-line anchor, `:N-M` for a range. 1-indexed for display.
fn format_line_range(thread: &Conversation) -> String {
    let start = thread.anchor.start_row + 1;
    let end = thread.anchor.end_row + 1;
    if start == end {
        format!(":{start}")
    } else {
        format!(":{start}-{end}")
    }
}

/// Writes `node` and every descendant into `out`, indenting by depth and
/// marking the targeted node with `►► ` so it stands out in the prompt.
fn append_subtree(
    out: &mut String,
    thread: &Conversation,
    node: &Comment,
    depth: usize,
    target: CommentId,
) {
    let indent = "  ".repeat(depth);
    let marker = if node.id == target { "►► " } else { "- " };
    let author = match &node.author {
        CommentAuthor::User => "User",
        CommentAuthor::Agent => "Agent",
        CommentAuthor::Remote { login } => login.as_str(),
    };
    let kind = match node.kind {
        CommentKind::Comment => "comment",
        CommentKind::Question => "question",
        CommentKind::Task => "task",
    };
    // A non-target node with at least one Agent child has already been
    // addressed; flag it so the agent doesn't relitigate handled branches.
    let replied_suffix = if node.id != target
        && thread
            .children(node.id)
            .into_iter()
            .any(|child| matches!(child.author, CommentAuthor::Agent))
    {
        " (replied)"
    } else {
        ""
    };
    // Indent multi-line bodies so they stay visually grouped under their node.
    let body = node.body.replace('\n', &format!("\n{indent}    "));
    out.push_str(&format!(
        "{indent}{marker}{author} ({kind}){replied_suffix}: {body}\n"
    ));
    for child in thread.children(node.id) {
        append_subtree(out, thread, child, depth + 1, target);
    }
}

/// Sends every open Task-kind thread to the agent as one batched message.
pub(crate) fn send_tasks_to_agent(
    workspace: &Workspace,
    store: Entity<CommentStore>,
    cx: &mut App,
) {
    let tasks: Vec<Conversation> = store
        .read(cx)
        .all_conversations()
        .filter(|thread| thread.kind == CommentKind::Task && thread.status == ConversationStatus::Open)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChangeId, CommentAnchor, ConversationId};
    use util::rel_path::RelPath;

    fn test_conversation() -> (Conversation, CommentId, CommentId) {
        let root = Comment::new(
            CommentAuthor::User,
            "Why is this unwrap safe?".into(),
            None,
            CommentKind::Question,
        );
        let agent_reply = Comment::new(
            CommentAuthor::Agent,
            "The index is bounds-checked above.".into(),
            Some(root.id),
            CommentKind::Comment,
        );
        let follow_up = Comment::new(
            CommentAuthor::User,
            "Please add a comment saying so.".into(),
            Some(root.id),
            CommentKind::Task,
        );
        let (root_id, follow_up_id) = (root.id, follow_up.id);
        let conversation = Conversation {
            id: ConversationId::new(),
            change_id: Some(ChangeId("42".into())),
            file: RelPath::unix("src/lib.rs").unwrap().into(),
            anchor: CommentAnchor {
                start_row: 9,
                end_row: 9,
                fingerprint: "let value = items[index];".into(),
                ..Default::default()
            },
            kind: CommentKind::Question,
            status: ConversationStatus::Open,
            nodes: vec![root, agent_reply, follow_up],
            collapsed: false,
        };
        (conversation, root_id, follow_up_id)
    }

    #[test]
    fn test_build_node_feedback_marks_target_and_replied_branches() {
        let (conversation, _root_id, follow_up_id) = test_conversation();
        let prompt = build_node_feedback(&conversation, follow_up_id).unwrap();

        assert!(prompt.starts_with("Please add a comment saying so."));
        assert!(prompt.contains("`src/lib.rs:10`"));
        assert!(prompt.contains("`let value = items[index];`"));
        assert!(prompt.contains("►► User (task): Please add a comment saying so."));
        assert!(
            prompt.contains("- User (question) (replied): Why is this unwrap safe?"),
            "root with an agent reply should be marked (replied): {prompt}"
        );
    }

    #[test]
    fn test_build_node_feedback_missing_node() {
        let (conversation, ..) = test_conversation();
        assert!(build_node_feedback(&conversation, CommentId::new()).is_none());
    }

    #[test]
    fn test_build_feedback_lists_every_node() {
        let (conversation, ..) = test_conversation();
        let feedback = build_feedback(&conversation);
        assert!(feedback.contains("`src/lib.rs` (lines 10-10)"));
        assert!(feedback.contains("- User: Why is this unwrap safe?"));
        assert!(feedback.contains("- Agent: The index is bounds-checked above."));
        assert!(feedback.contains("- User: Please add a comment saying so."));
    }
}
