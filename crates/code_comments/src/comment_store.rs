//! The in-memory model for inline code comments and the per-workspace
//! [`CommentStore`] entity that owns it and persists it to [`CommentsDb`].

use std::{sync::Arc, time::Duration};

use collections::HashMap;
use gpui::{AppContext as _, Context, EventEmitter, Task};
use util::{ResultExt as _, rel_path::RelPath};
use uuid::Uuid;
use workspace::WorkspaceId;

use crate::persistence::{CommentsDb, DbNodeRow, DbThreadRow};

const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Stable identifier for a comment thread.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ThreadId(pub Uuid);

/// Stable identifier for a single comment node within a thread.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CommentId(pub Uuid);

impl ThreadId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ThreadId {
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
pub enum CommentStatus {
    #[default]
    Open,
    Resolved,
}

impl CommentStatus {
    fn to_db(self) -> i64 {
        match self {
            CommentStatus::Open => 0,
            CommentStatus::Resolved => 1,
        }
    }

    fn from_db(value: i64) -> Self {
        match value {
            1 => CommentStatus::Resolved,
            _ => CommentStatus::Open,
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

/// The persisted position of a thread: a row/column range plus a text
/// fingerprint of the anchored line(s) used for best-effort re-anchoring.
#[derive(Clone, Debug, Default)]
pub struct CommentAnchor {
    pub start_row: u32,
    pub start_column: u32,
    pub end_row: u32,
    pub end_column: u32,
    /// Trimmed text of the anchored line(s) at creation time.
    pub fingerprint: String,
}

/// A single message within a thread's comment tree.
#[derive(Clone, Debug)]
pub struct CommentNode {
    pub id: CommentId,
    /// `None` for the thread's root comment.
    pub parent_id: Option<CommentId>,
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

impl CommentNode {
    pub fn new(
        author: CommentAuthor,
        body: String,
        parent_id: Option<CommentId>,
        kind: CommentKind,
    ) -> Self {
        Self {
            id: CommentId::new(),
            parent_id,
            author,
            body,
            kind,
            created_at: now_timestamp(),
            remote_id: None,
        }
    }
}

/// A comment thread: a root comment plus a tree of replies, anchored to a
/// range in one file.
#[derive(Clone, Debug)]
pub struct CommentThread {
    pub id: ThreadId,
    /// Worktree-relative path of the commented file.
    pub file: Arc<RelPath>,
    pub anchor: CommentAnchor,
    pub kind: CommentKind,
    pub status: CommentStatus,
    /// Flat node list; the tree is formed via `CommentNode::parent_id`.
    pub nodes: Vec<CommentNode>,
    pub collapsed: bool,
}

impl CommentThread {
    /// The root comment of the thread, if it has one.
    pub fn root(&self) -> Option<&CommentNode> {
        self.nodes.iter().find(|node| node.parent_id.is_none())
    }

    /// Direct replies to the given node, oldest first.
    pub fn children<'a>(&'a self, parent: CommentId) -> impl Iterator<Item = &'a CommentNode> {
        self.nodes
            .iter()
            .filter(move |node| node.parent_id == Some(parent))
    }

    fn to_db_rows(&self) -> (DbThreadRow, Vec<DbNodeRow>) {
        let thread_row = DbThreadRow {
            thread_id: self.id.0.to_string(),
            path: self.file.to_proto(),
            start_row: self.anchor.start_row,
            start_column: self.anchor.start_column,
            end_row: self.anchor.end_row,
            end_column: self.anchor.end_column,
            fingerprint: self.anchor.fingerprint.clone(),
            kind: self.kind.to_db(),
            status: self.status.to_db(),
            collapsed: self.collapsed,
        };
        let node_rows = self
            .nodes
            .iter()
            .map(|node| {
                let (author_kind, author_login) = node.author.to_db();
                DbNodeRow {
                    thread_id: self.id.0.to_string(),
                    node_id: node.id.0.to_string(),
                    parent_id: node.parent_id.map(|id| id.0.to_string()),
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
    threads_by_file: HashMap<Arc<RelPath>, Vec<CommentThread>>,
    save_task: Option<Task<()>>,
}

impl EventEmitter<CommentStoreEvent> for CommentStore {}

impl CommentStore {
    /// Creates the store for a workspace and asynchronously loads any
    /// previously persisted comments.
    pub fn new(workspace_id: WorkspaceId, cx: &mut Context<Self>) -> Self {
        let mut store = Self {
            workspace_id,
            threads_by_file: HashMap::default(),
            save_task: None,
        };
        store.load(cx);
        store
    }

    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    /// Threads anchored in the given file, in insertion order.
    pub fn threads_for_file(&self, path: &RelPath) -> &[CommentThread] {
        self.threads_by_file
            .get(path)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Every thread across every file.
    pub fn all_threads(&self) -> impl Iterator<Item = &CommentThread> {
        self.threads_by_file.values().flatten()
    }

    pub fn thread(&self, id: ThreadId) -> Option<&CommentThread> {
        self.all_threads().find(|thread| thread.id == id)
    }

    fn thread_mut(&mut self, id: ThreadId) -> Option<&mut CommentThread> {
        self.threads_by_file
            .values_mut()
            .flatten()
            .find(|thread| thread.id == id)
    }

    /// Inserts a new thread or replaces an existing one with the same id.
    pub fn upsert_thread(&mut self, thread: CommentThread, cx: &mut Context<Self>) {
        let list = self.threads_by_file.entry(thread.file.clone()).or_default();
        if let Some(existing) = list.iter_mut().find(|t| t.id == thread.id) {
            *existing = thread;
        } else {
            list.push(thread);
        }
        self.changed(cx);
    }

    /// Removes a thread and all of its nodes.
    pub fn remove_thread(&mut self, id: ThreadId, cx: &mut Context<Self>) {
        let mut removed = false;
        for list in self.threads_by_file.values_mut() {
            let before = list.len();
            list.retain(|thread| thread.id != id);
            removed |= list.len() != before;
        }
        if removed {
            self.threads_by_file.retain(|_, list| !list.is_empty());
            self.changed(cx);
        }
    }

    /// Appends a node (a reply or an agent answer) to a thread.
    pub fn add_node(&mut self, thread_id: ThreadId, node: CommentNode, cx: &mut Context<Self>) {
        if let Some(thread) = self.thread_mut(thread_id) {
            thread.nodes.push(node);
            self.changed(cx);
        }
    }

    /// Replaces the body of an existing node.
    pub fn update_node_body(
        &mut self,
        thread_id: ThreadId,
        node_id: CommentId,
        body: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread) = self.thread_mut(thread_id)
            && let Some(node) = thread.nodes.iter_mut().find(|node| node.id == node_id)
        {
            node.body = body;
            self.changed(cx);
        }
    }

    /// Sets a thread's resolved/open status.
    pub fn set_thread_status(
        &mut self,
        thread_id: ThreadId,
        status: CommentStatus,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread) = self.thread_mut(thread_id) {
            thread.status = status;
            self.changed(cx);
        }
    }

    /// Sets a thread's comment kind (Comment / Question / Task).
    pub fn set_thread_kind(
        &mut self,
        thread_id: ThreadId,
        kind: CommentKind,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread) = self.thread_mut(thread_id) {
            thread.kind = kind;
            self.changed(cx);
        }
    }

    /// Sets a single node's kind. Independent of the thread-level kind so an
    /// individual reply can be tagged as a Question/Task (e.g. for an agent
    /// "handled in <commit>" answer) without changing the whole thread.
    pub fn set_node_kind(
        &mut self,
        thread_id: ThreadId,
        node_id: CommentId,
        kind: CommentKind,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread) = self.thread_mut(thread_id)
            && let Some(node) = thread.nodes.iter_mut().find(|node| node.id == node_id)
        {
            node.kind = kind;
            self.changed(cx);
        }
    }

    /// Sets a thread's collapsed/expanded state.
    pub fn set_thread_collapsed(
        &mut self,
        thread_id: ThreadId,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread) = self.thread_mut(thread_id) {
            thread.collapsed = collapsed;
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

    fn to_db_rows(&self) -> (Vec<DbThreadRow>, Vec<DbNodeRow>) {
        let mut threads = Vec::new();
        let mut nodes = Vec::new();
        for thread in self.all_threads() {
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
                    let threads = db.load_threads(workspace_id)?;
                    let nodes = db.load_nodes(workspace_id)?;
                    anyhow::Ok((threads, nodes))
                })
                .await;
            match loaded {
                Ok((threads, nodes)) => {
                    this.update(cx, |this, cx| {
                        this.threads_by_file = build_index(threads, nodes);
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
    threads: Vec<DbThreadRow>,
    nodes: Vec<DbNodeRow>,
) -> HashMap<Arc<RelPath>, Vec<CommentThread>> {
    let mut nodes_by_thread: HashMap<String, Vec<CommentNode>> = HashMap::default();
    for row in nodes {
        let Some(node) = node_from_row(&row) else {
            continue;
        };
        nodes_by_thread
            .entry(row.thread_id)
            .or_default()
            .push(node);
    }

    let mut index: HashMap<Arc<RelPath>, Vec<CommentThread>> = HashMap::default();
    for row in threads {
        let Ok(id) = Uuid::parse_str(&row.thread_id) else {
            log::error!("skipping comment thread with invalid id {:?}", row.thread_id);
            continue;
        };
        let Ok(file) = RelPath::from_proto(&row.path) else {
            log::error!("skipping comment thread with invalid path {:?}", row.path);
            continue;
        };
        let thread = CommentThread {
            id: ThreadId(id),
            file: file.clone(),
            anchor: CommentAnchor {
                start_row: row.start_row,
                start_column: row.start_column,
                end_row: row.end_row,
                end_column: row.end_column,
                fingerprint: row.fingerprint,
            },
            kind: CommentKind::from_db(row.kind),
            status: CommentStatus::from_db(row.status),
            nodes: nodes_by_thread.remove(&row.thread_id).unwrap_or_default(),
            collapsed: row.collapsed,
        };
        index.entry(file).or_default().push(thread);
    }
    index
}

fn node_from_row(row: &DbNodeRow) -> Option<CommentNode> {
    let id = Uuid::parse_str(&row.node_id)
        .map_err(|_| log::error!("skipping comment node with invalid id {:?}", row.node_id))
        .ok()?;
    let parent_id = match &row.parent_id {
        Some(parent) => Some(CommentId(Uuid::parse_str(parent).ok()?)),
        None => None,
    };
    Some(CommentNode {
        id: CommentId(id),
        parent_id,
        author: CommentAuthor::from_db(row.author_kind, row.author_login.clone()),
        body: row.body.clone(),
        kind: CommentKind::from_db(row.kind),
        created_at: row.created_at,
        remote_id: row.remote_id,
    })
}
