//! The in-memory model for inline code comments and the per-workspace
//! [`CommentStore`] entity that owns it and persists it to [`CommentsDb`].

use std::{sync::Arc, time::Duration};

use collections::HashMap;
use gpui::{AppContext as _, Context, EventEmitter, Task};
use util::{ResultExt as _, rel_path::RelPath};
use uuid::Uuid;
use workspace::WorkspaceId;

use crate::persistence::{CommentsDb, DbCommentRow, DbConversationRow};

const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Stable identifier for a comment thread.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ConversationId(pub Uuid);

/// Stable identifier for a single comment node within a thread.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CommentId(pub Uuid);

impl ConversationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ConversationId {
    fn default() -> Self {
        Self::new()
    }
}

impl CommentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CommentId {
    fn default() -> Self {
        Self::new()
    }
}

/// The behavior a comment requests from the agent.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CommentKind {
    /// A plain note; no agent involvement.
    #[default]
    Comment,
    /// Sent to the agent on submission; its answer is appended to the tree.
    Question,
    /// Queued; delivered to the agent in a batch via `SendTasksToAgent`.
    Task,
}

impl CommentKind {
    fn to_db(self) -> i64 {
        match self {
            CommentKind::Comment => 0,
            CommentKind::Question => 1,
            CommentKind::Task => 2,
        }
    }

    fn from_db(value: i64) -> Self {
        match value {
            1 => CommentKind::Question,
            2 => CommentKind::Task,
            _ => CommentKind::Comment,
        }
    }
}

/// Whether a comment thread is still open or has been resolved by the user.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ConversationStatus {
    #[default]
    Open,
    Resolved,
}

impl ConversationStatus {
    fn to_db(self) -> i64 {
        match self {
            ConversationStatus::Open => 0,
            ConversationStatus::Resolved => 1,
        }
    }

    fn from_db(value: i64) -> Self {
        match value {
            1 => ConversationStatus::Resolved,
            _ => ConversationStatus::Open,
        }
    }
}

/// Who authored a comment node.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommentAuthor {
    /// The local user.
    User,
    /// The active agent (a reply to a question, etc.).
    Agent,
    /// A comment pulled from a remote provider, attributed to its login.
    Remote { login: String },
}

impl CommentAuthor {
    fn to_db(&self) -> (i64, Option<String>) {
        match self {
            CommentAuthor::User => (0, None),
            CommentAuthor::Agent => (1, None),
            CommentAuthor::Remote { login } => (2, Some(login.clone())),
        }
    }

    fn from_db(kind: i64, login: Option<String>) -> Self {
        match kind {
            1 => CommentAuthor::Agent,
            2 => CommentAuthor::Remote {
                login: login.unwrap_or_default(),
            },
            _ => CommentAuthor::User,
        }
    }
}

/// Opaque identifier for a code-review change (PR / MR / CL / change-set).
/// The provider defines what this string means; we never parse it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ChangeId(pub String);

/// Opaque identifier for a specific revision of a change (commit SHA,
/// patchset id, snapshot ref — provider-defined). Used by the re-anchoring
/// path to ask the local VCS where a comment's anchored line is now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct RevisionId(pub String);

/// The persisted position of a conversation: a row/column range plus a text
/// fingerprint of the anchored line(s) used for best-effort re-anchoring,
/// and the revision the anchor was originally computed against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommentAnchor {
    pub start_row: u32,
    pub start_column: u32,
    pub end_row: u32,
    pub end_column: u32,
    /// Trimmed text of the anchored line(s) at creation time.
    pub fingerprint: String,
    /// Revision the comment was anchored against, when known. `None` for
    /// purely local comments that were never tied to a provider revision.
    pub revision: Option<RevisionId>,
}

/// A single message within a thread's comment tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub id: CommentId,
    /// `None` for the thread's root comment.
    pub in_reply_to: Option<CommentId>,
    pub author: CommentAuthor,
    pub body: String,
    /// Per-node classification. Lets individual replies be tagged as a
    /// Question or Task even when the rest of the thread is something else
    /// (so an agent can answer with a Task-tagged "handled in <commit>" reply).
    pub kind: CommentKind,
    /// Unix timestamp (seconds).
    pub created_at: i64,
    /// Identifier on the remote provider, once synced.
    pub remote_id: Option<i64>,
}

impl Comment {
    pub fn new(
        author: CommentAuthor,
        body: String,
        in_reply_to: Option<CommentId>,
        kind: CommentKind,
    ) -> Self {
        Self {
            id: CommentId::new(),
            in_reply_to,
            author,
            body,
            kind,
            created_at: now_timestamp(),
            remote_id: None,
        }
    }
}

/// A conversation: a root comment plus a tree of replies, anchored to a
/// range in one file and (when synced with a provider) attached to a
/// specific code-review change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub id: ConversationId,
    /// Code-review change this conversation belongs to (PR / MR / CL).
    /// `None` for purely local conversations not associated with a change.
    pub change_id: Option<ChangeId>,
    /// Worktree-relative path of the commented file.
    pub file: Arc<RelPath>,
    pub anchor: CommentAnchor,
    pub kind: CommentKind,
    pub status: ConversationStatus,
    /// Flat node list; the tree is formed via `Comment::in_reply_to`.
    pub nodes: Vec<Comment>,
    pub collapsed: bool,
}

impl Conversation {
    /// The root comment of the thread, if it has one.
    pub fn root(&self) -> Option<&Comment> {
        self.nodes.iter().find(|node| node.in_reply_to.is_none())
    }

    /// Direct replies to the given node, oldest first.
    pub fn children(&self, parent: CommentId) -> impl Iterator<Item = &Comment> {
        self.nodes
            .iter()
            .filter(move |node| node.in_reply_to == Some(parent))
    }

    fn to_db_rows(&self) -> (DbConversationRow, Vec<DbCommentRow>) {
        let thread_row = DbConversationRow {
            conversation_id: self.id.0.to_string(),
            change_id: self.change_id.as_ref().map(|id| id.0.clone()),
            path: self.file.to_proto(),
            start_row: self.anchor.start_row,
            start_column: self.anchor.start_column,
            end_row: self.anchor.end_row,
            end_column: self.anchor.end_column,
            fingerprint: self.anchor.fingerprint.clone(),
            anchor_revision: self.anchor.revision.as_ref().map(|r| r.0.clone()),
            kind: self.kind.to_db(),
            status: self.status.to_db(),
            collapsed: self.collapsed,
        };
        let node_rows = self
            .nodes
            .iter()
            .map(|node| {
                let (author_kind, author_login) = node.author.to_db();
                DbCommentRow {
                    conversation_id: self.id.0.to_string(),
                    comment_id: node.id.0.to_string(),
                    in_reply_to: node.in_reply_to.map(|id| id.0.to_string()),
                    author_kind,
                    author_login,
                    body: node.body.clone(),
                    created_at: node.created_at,
                    remote_id: node.remote_id,
                    kind: node.kind.to_db(),
                }
            })
            .collect();
        (thread_row, node_rows)
    }
}

/// Emitted whenever any thread or node changes; editors observe this to
/// refresh their rendered comment cards.
#[derive(Clone, Copy, Debug)]
pub enum CommentStoreEvent {
    Changed,
}

/// The per-workspace owner of all comment threads. Loads from and persists to
/// [`CommentsDb`], and notifies observers on every change.
pub struct CommentStore {
    workspace_id: WorkspaceId,
    conversations_by_file: HashMap<Arc<RelPath>, Vec<Conversation>>,
    save_task: Option<Task<()>>,
}

impl EventEmitter<CommentStoreEvent> for CommentStore {}

impl CommentStore {
    /// Creates the store for a workspace and asynchronously loads any
    /// previously persisted comments.
    pub fn new(workspace_id: WorkspaceId, cx: &mut Context<Self>) -> Self {
        let mut store = Self {
            workspace_id,
            conversations_by_file: HashMap::default(),
            save_task: None,
        };
        store.load(cx);
        store
    }

    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    /// Threads anchored in the given file, in insertion order.
    pub fn conversations_for_file(&self, path: &RelPath) -> &[Conversation] {
        self.conversations_by_file
            .get(path)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Every thread across every file.
    pub fn all_conversations(&self) -> impl Iterator<Item = &Conversation> {
        self.conversations_by_file.values().flatten()
    }

    pub fn thread(&self, id: ConversationId) -> Option<&Conversation> {
        self.all_conversations().find(|thread| thread.id == id)
    }

    fn conversation_mut(&mut self, id: ConversationId) -> Option<&mut Conversation> {
        self.conversations_by_file
            .values_mut()
            .flatten()
            .find(|thread| thread.id == id)
    }

    /// Inserts a new thread or replaces an existing one with the same id.
    pub fn upsert_conversation(&mut self, thread: Conversation, cx: &mut Context<Self>) {
        let list = self.conversations_by_file.entry(thread.file.clone()).or_default();
        if let Some(existing) = list.iter_mut().find(|t| t.id == thread.id) {
            *existing = thread;
        } else {
            list.push(thread);
        }
        self.changed(cx);
    }

    /// Removes a thread from the index and returns it, without persisting or
    /// notifying; callers reinsert it (possibly under another file) and then
    /// signal the change.
    fn take_conversation(&mut self, id: ConversationId) -> Option<Conversation> {
        for list in self.conversations_by_file.values_mut() {
            if let Some(index) = list.iter().position(|thread| thread.id == id) {
                let conversation = list.remove(index);
                self.conversations_by_file.retain(|_, list| !list.is_empty());
                return Some(conversation);
            }
        }
        None
    }

    /// Removes a thread and all of its nodes.
    pub fn remove_conversation(&mut self, id: ConversationId, cx: &mut Context<Self>) {
        let mut removed = false;
        for list in self.conversations_by_file.values_mut() {
            let before = list.len();
            list.retain(|thread| thread.id != id);
            removed |= list.len() != before;
        }
        if removed {
            self.conversations_by_file.retain(|_, list| !list.is_empty());
            self.changed(cx);
        }
    }

    /// Appends a node (a reply or an agent answer) to a thread.
    pub fn add_comment(&mut self, conversation_id: ConversationId, node: Comment, cx: &mut Context<Self>) {
        if let Some(thread) = self.conversation_mut(conversation_id) {
            thread.nodes.push(node);
            self.changed(cx);
        }
    }

    /// Replaces the body of an existing node.
    pub fn update_comment_body(
        &mut self,
        conversation_id: ConversationId,
        comment_id: CommentId,
        body: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread) = self.conversation_mut(conversation_id)
            && let Some(node) = thread.nodes.iter_mut().find(|node| node.id == comment_id)
        {
            node.body = body;
            self.changed(cx);
        }
    }

    /// Sets a thread's resolved/open status.
    pub fn set_conversation_status(
        &mut self,
        conversation_id: ConversationId,
        status: ConversationStatus,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread) = self.conversation_mut(conversation_id) {
            thread.status = status;
            self.changed(cx);
        }
    }

    /// Sets a thread's comment kind (Comment / Question / Task).
    pub fn set_conversation_kind(
        &mut self,
        conversation_id: ConversationId,
        kind: CommentKind,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread) = self.conversation_mut(conversation_id) {
            thread.kind = kind;
            self.changed(cx);
        }
    }

    /// Sets a single node's kind. Independent of the thread-level kind so an
    /// individual reply can be tagged as a Question/Task (e.g. for an agent
    /// "handled in <commit>" answer) without changing the whole thread.
    pub fn set_comment_kind(
        &mut self,
        conversation_id: ConversationId,
        comment_id: CommentId,
        kind: CommentKind,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread) = self.conversation_mut(conversation_id)
            && let Some(node) = thread.nodes.iter_mut().find(|node| node.id == comment_id)
        {
            node.kind = kind;
            self.changed(cx);
        }
    }

    /// Sets a thread's collapsed/expanded state.
    pub fn set_conversation_collapsed(
        &mut self,
        conversation_id: ConversationId,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread) = self.conversation_mut(conversation_id) {
            thread.collapsed = collapsed;
            self.changed(cx);
        }
    }

    /// Merges conversations fetched from a remote provider into the store.
    ///
    /// Fetched conversations are matched to existing ones by their root
    /// comment's `remote_id` (within the same change), so repeated fetches
    /// update in place instead of duplicating. Local-only state survives the
    /// merge: replies that haven't been synced, per-node kinds, and the
    /// collapsed flag are kept, while remote-owned state (bodies, anchors,
    /// resolved status) is taken from the fetched copy. Conversations that no
    /// longer appear in the fetch are kept; deleting local data based on a
    /// possibly-partial fetch is not safe.
    pub fn merge_remote_conversations(
        &mut self,
        change_id: &ChangeId,
        fetched: Vec<Conversation>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        for conversation in fetched {
            let Some(root_remote_id) = conversation.root().and_then(|root| root.remote_id) else {
                log::warn!(
                    "code_comments: skipping fetched conversation without a remote root id"
                );
                continue;
            };
            let existing = self
                .all_conversations()
                .find(|existing| {
                    existing.change_id.as_ref() == Some(change_id)
                        && existing
                            .root()
                            .is_some_and(|root| root.remote_id == Some(root_remote_id))
                })
                .map(|existing| existing.id);
            match existing {
                // Take the conversation out and reinsert it after merging:
                // the fetched copy may live under a different file.
                Some(existing_id) => {
                    if let Some(mut existing) = self.take_conversation(existing_id) {
                        changed |= merge_fetched_conversation(&mut existing, conversation);
                        self.conversations_by_file
                            .entry(existing.file.clone())
                            .or_default()
                            .push(existing);
                    }
                }
                None => {
                    self.conversations_by_file
                        .entry(conversation.file.clone())
                        .or_default()
                        .push(conversation);
                    changed = true;
                }
            }
        }
        if changed {
            self.changed(cx);
        }
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_save(cx);
        cx.emit(CommentStoreEvent::Changed);
        cx.notify();
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let workspace_id = self.workspace_id;
        let (threads, nodes) = self.to_db_rows();
        let db = CommentsDb::global(cx);
        let executor = cx.background_executor().clone();
        self.save_task = Some(cx.background_spawn(async move {
            executor.timer(SAVE_DEBOUNCE).await;
            db.replace_all(workspace_id, threads, nodes)
                .await
                .log_err();
        }));
    }

    fn to_db_rows(&self) -> (Vec<DbConversationRow>, Vec<DbCommentRow>) {
        let mut threads = Vec::new();
        let mut nodes = Vec::new();
        for thread in self.all_conversations() {
            let (thread_row, mut node_rows) = thread.to_db_rows();
            threads.push(thread_row);
            nodes.append(&mut node_rows);
        }
        (threads, nodes)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let workspace_id = self.workspace_id;
        let db = CommentsDb::global(cx);
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let threads = db.load_conversations(workspace_id)?;
                    let nodes = db.load_comments(workspace_id)?;
                    anyhow::Ok((threads, nodes))
                })
                .await;
            match loaded {
                Ok((threads, nodes)) => {
                    this.update(cx, |this, cx| {
                        this.conversations_by_file = build_index(threads, nodes);
                        cx.emit(CommentStoreEvent::Changed);
                        cx.notify();
                    })
                    .log_err();
                }
                Err(err) => log::error!("failed to load code comments: {err:#}"),
            }
        })
        .detach();
    }
}

fn now_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Rebuilds the in-memory thread index from rows loaded out of the database.
fn build_index(
    threads: Vec<DbConversationRow>,
    nodes: Vec<DbCommentRow>,
) -> HashMap<Arc<RelPath>, Vec<Conversation>> {
    let mut nodes_by_thread: HashMap<String, Vec<Comment>> = HashMap::default();
    for row in nodes {
        let Some(node) = comment_from_row(&row) else {
            continue;
        };
        nodes_by_thread
            .entry(row.conversation_id)
            .or_default()
            .push(node);
    }

    let mut index: HashMap<Arc<RelPath>, Vec<Conversation>> = HashMap::default();
    for row in threads {
        let Ok(id) = Uuid::parse_str(&row.conversation_id) else {
            log::error!("skipping comment thread with invalid id {:?}", row.conversation_id);
            continue;
        };
        let Ok(file) = RelPath::from_proto(&row.path) else {
            log::error!("skipping comment thread with invalid path {:?}", row.path);
            continue;
        };
        let thread = Conversation {
            id: ConversationId(id),
            change_id: row.change_id.map(ChangeId),
            file: file.clone(),
            anchor: CommentAnchor {
                start_row: row.start_row,
                start_column: row.start_column,
                end_row: row.end_row,
                end_column: row.end_column,
                fingerprint: row.fingerprint,
                revision: row.anchor_revision.map(RevisionId),
            },
            kind: CommentKind::from_db(row.kind),
            status: ConversationStatus::from_db(row.status),
            nodes: nodes_by_thread.remove(&row.conversation_id).unwrap_or_default(),
            collapsed: row.collapsed,
        };
        index.entry(file).or_default().push(thread);
    }
    index
}

/// Merges a freshly fetched copy of a conversation into the locally stored
/// one, matching comments by `remote_id`. Remote-owned state (file, anchor,
/// resolved status, comment bodies and authors) comes from the fetched copy;
/// local-only state (the conversation id, collapsed flag, per-node kinds, and
/// unsynced local replies) is preserved. Returns whether anything changed.
fn merge_fetched_conversation(existing: &mut Conversation, fetched: Conversation) -> bool {
    let snapshot = existing.clone();

    existing.file = fetched.file;
    existing.anchor = fetched.anchor;
    existing.status = fetched.status;

    let existing_by_remote: HashMap<i64, (CommentId, CommentKind)> = snapshot
        .nodes
        .iter()
        .filter_map(|node| Some((node.remote_id?, (node.id, node.kind))))
        .collect();

    // First pass: decide each fetched node's final id, so replies can be
    // re-pointed even when their parent appears later in the list.
    let mut final_ids: HashMap<CommentId, CommentId> = HashMap::default();
    for node in &fetched.nodes {
        let final_id = node
            .remote_id
            .and_then(|remote_id| existing_by_remote.get(&remote_id))
            .map(|(id, _)| *id)
            .unwrap_or(node.id);
        final_ids.insert(node.id, final_id);
    }

    let mut merged: Vec<Comment> = fetched
        .nodes
        .into_iter()
        .map(|mut node| {
            if let Some(remote_id) = node.remote_id
                && let Some((_, kind)) = existing_by_remote.get(&remote_id)
            {
                node.kind = *kind;
            }
            node.in_reply_to = node
                .in_reply_to
                .and_then(|parent| final_ids.get(&parent).copied());
            node.id = final_ids[&node.id];
            node
        })
        .collect();

    let root_id = merged
        .iter()
        .find(|node| node.in_reply_to.is_none())
        .map(|node| node.id);
    let merged_ids: collections::HashSet<CommentId> =
        merged.iter().map(|node| node.id).collect();
    let mut local_nodes: Vec<Comment> = snapshot
        .nodes
        .iter()
        .filter(|node| node.remote_id.is_none())
        .cloned()
        .collect();
    let local_ids: collections::HashSet<CommentId> =
        local_nodes.iter().map(|node| node.id).collect();
    for node in &mut local_nodes {
        // A local reply whose remote parent disappeared from the fetch would
        // become invisible; reattach it to the root instead.
        if let Some(parent) = node.in_reply_to
            && !merged_ids.contains(&parent)
            && !local_ids.contains(&parent)
        {
            node.in_reply_to = root_id;
        }
    }
    merged.extend(local_nodes);
    existing.nodes = merged;

    *existing != snapshot
}

fn comment_from_row(row: &DbCommentRow) -> Option<Comment> {
    let id = Uuid::parse_str(&row.comment_id)
        .map_err(|_| log::error!("skipping comment node with invalid id {:?}", row.comment_id))
        .ok()?;
    let in_reply_to = match &row.in_reply_to {
        Some(parent) => Some(CommentId(Uuid::parse_str(parent).ok()?)),
        None => None,
    };
    Some(Comment {
        id: CommentId(id),
        in_reply_to,
        author: CommentAuthor::from_db(row.author_kind, row.author_login.clone()),
        body: row.body.clone(),
        kind: CommentKind::from_db(row.kind),
        created_at: row.created_at,
        remote_id: row.remote_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_comment(remote_id: i64, in_reply_to: Option<CommentId>, body: &str) -> Comment {
        Comment {
            id: CommentId::new(),
            in_reply_to,
            author: CommentAuthor::Remote {
                login: "alice".into(),
            },
            body: body.to_string(),
            kind: CommentKind::Comment,
            created_at: 0,
            remote_id: Some(remote_id),
        }
    }

    fn conversation(nodes: Vec<Comment>) -> Conversation {
        Conversation {
            id: ConversationId::new(),
            change_id: Some(ChangeId("42".into())),
            file: RelPath::unix("src/lib.rs").unwrap().into(),
            anchor: CommentAnchor::default(),
            kind: CommentKind::Comment,
            status: ConversationStatus::Open,
            nodes,
            collapsed: false,
        }
    }

    #[test]
    fn test_merge_updates_remote_state_and_keeps_local_state() {
        let root = remote_comment(1, None, "old body");
        let mut local_reply = Comment::new(
            CommentAuthor::User,
            "local reply".into(),
            Some(root.id),
            CommentKind::Task,
        );
        local_reply.created_at = 5;
        let mut existing = conversation(vec![root.clone(), local_reply.clone()]);
        existing.collapsed = true;
        existing.nodes[0].kind = CommentKind::Question;

        let mut fetched_root = remote_comment(1, None, "edited body");
        let fetched_reply = remote_comment(2, Some(fetched_root.id), "remote reply");
        fetched_root.created_at = 1;
        let mut fetched = conversation(vec![fetched_root, fetched_reply]);
        fetched.status = ConversationStatus::Resolved;
        fetched.anchor.start_row = 7;

        assert!(merge_fetched_conversation(&mut existing, fetched));

        assert_eq!(existing.status, ConversationStatus::Resolved);
        assert_eq!(existing.anchor.start_row, 7);
        assert!(existing.collapsed);
        let merged_root = existing.root().unwrap();
        assert_eq!(merged_root.id, root.id, "matched root keeps its local id");
        assert_eq!(merged_root.body, "edited body");
        assert_eq!(
            merged_root.kind,
            CommentKind::Question,
            "local kind survives the merge"
        );
        let children: Vec<_> = existing.children(root.id).collect();
        assert_eq!(children.len(), 2);
        assert!(children.iter().any(|node| node.body == "remote reply"));
        assert!(children.iter().any(|node| node.id == local_reply.id));
    }

    #[test]
    fn test_merge_reattaches_local_reply_to_root_when_parent_disappears() {
        let root = remote_comment(1, None, "root");
        let removed = remote_comment(2, Some(root.id), "removed remotely");
        let local_reply = Comment::new(
            CommentAuthor::User,
            "reply to removed".into(),
            Some(removed.id),
            CommentKind::Comment,
        );
        let mut existing = conversation(vec![root.clone(), removed, local_reply.clone()]);

        let fetched = conversation(vec![remote_comment(1, None, "root")]);
        assert!(merge_fetched_conversation(&mut existing, fetched));

        let reattached = existing
            .nodes
            .iter()
            .find(|node| node.id == local_reply.id)
            .unwrap();
        assert_eq!(reattached.in_reply_to, Some(root.id));
    }

    #[test]
    fn test_merge_is_idempotent() {
        let root = remote_comment(1, None, "root");
        let reply = remote_comment(2, Some(root.id), "reply");
        let mut existing = conversation(vec![root, reply]);
        let fetched = existing.clone();

        assert!(!merge_fetched_conversation(&mut existing, fetched.clone()));
        assert_eq!(existing, fetched);
    }
}
