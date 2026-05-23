//! Workspace-local sqlite persistence for inline code comments.
//!
//! Mirrors the breakpoint / `file_folds` persistence pattern: conversations
//! and their comments are stored keyed by `workspace_id`, with positions kept
//! as row/column pairs plus a text fingerprint so they can be re-anchored
//! after the file changes.

use anyhow::Result;
use db::{
    query,
    sqlez::{
        bindable::{Bind, Column, StaticColumnCount},
        statement::Statement,
    },
    sqlez_macros::sql,
};
use workspace::{WorkspaceDb, WorkspaceId};

/// One persisted conversation (without its `workspace_id`, which is implied
/// by the query).
#[derive(Clone, Debug)]
pub struct DbConversationRow {
    pub conversation_id: String,
    pub path: String,
    pub start_row: u32,
    pub start_column: u32,
    pub end_row: u32,
    pub end_column: u32,
    pub fingerprint: String,
    pub kind: i64,
    pub status: i64,
    pub collapsed: bool,
}

/// One persisted comment (a single message within a conversation's tree).
#[derive(Clone, Debug)]
pub struct DbCommentRow {
    pub conversation_id: String,
    pub comment_id: String,
    pub in_reply_to: Option<String>,
    pub author_kind: i64,
    pub author_login: Option<String>,
    pub body: String,
    pub created_at: i64,
    pub remote_id: Option<i64>,
    /// Per-comment CommentKind (Comment / Question / Task). Defaults to 0 for
    /// rows persisted before this column existed.
    pub kind: i64,
}

impl StaticColumnCount for DbConversationRow {
    fn column_count() -> usize {
        10
    }
}

impl Bind for DbConversationRow {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        let next = statement.bind(&self.conversation_id, start_index)?;
        let next = statement.bind(&self.path, next)?;
        let next = statement.bind(&self.start_row, next)?;
        let next = statement.bind(&self.start_column, next)?;
        let next = statement.bind(&self.end_row, next)?;
        let next = statement.bind(&self.end_column, next)?;
        let next = statement.bind(&self.fingerprint, next)?;
        let next = statement.bind(&self.kind, next)?;
        let next = statement.bind(&self.status, next)?;
        let next = statement.bind(&self.collapsed, next)?;
        Ok(next)
    }
}

impl Column for DbConversationRow {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (conversation_id, next) = Column::column(statement, start_index)?;
        let (path, next) = Column::column(statement, next)?;
        let (start_row, next) = Column::column(statement, next)?;
        let (start_column, next) = Column::column(statement, next)?;
        let (end_row, next) = Column::column(statement, next)?;
        let (end_column, next) = Column::column(statement, next)?;
        let (fingerprint, next) = Column::column(statement, next)?;
        let (kind, next) = Column::column(statement, next)?;
        let (status, next) = Column::column(statement, next)?;
        let (collapsed, next) = Column::column(statement, next)?;
        Ok((
            Self {
                conversation_id,
                path,
                start_row,
                start_column,
                end_row,
                end_column,
                fingerprint,
                kind,
                status,
                collapsed,
            },
            next,
        ))
    }
}

impl StaticColumnCount for DbCommentRow {
    fn column_count() -> usize {
        9
    }
}

impl Bind for DbCommentRow {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        let next = statement.bind(&self.conversation_id, start_index)?;
        let next = statement.bind(&self.comment_id, next)?;
        let next = statement.bind(&self.in_reply_to, next)?;
        let next = statement.bind(&self.author_kind, next)?;
        let next = statement.bind(&self.author_login, next)?;
        let next = statement.bind(&self.body, next)?;
        let next = statement.bind(&self.created_at, next)?;
        let next = statement.bind(&self.remote_id, next)?;
        let next = statement.bind(&self.kind, next)?;
        Ok(next)
    }
}

impl Column for DbCommentRow {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (conversation_id, next) = Column::column(statement, start_index)?;
        let (comment_id, next) = Column::column(statement, next)?;
        let (in_reply_to, next) = Column::column(statement, next)?;
        let (author_kind, next) = Column::column(statement, next)?;
        let (author_login, next) = Column::column(statement, next)?;
        let (body, next) = Column::column(statement, next)?;
        let (created_at, next) = Column::column(statement, next)?;
        let (remote_id, next) = Column::column(statement, next)?;
        let (kind, next) = Column::column(statement, next)?;
        Ok((
            Self {
                conversation_id,
                comment_id,
                in_reply_to,
                author_kind,
                author_login,
                body,
                created_at,
                remote_id,
                kind,
            },
            next,
        ))
    }
}

pub struct CommentsDb(db::sqlez::thread_safe_connection::ThreadSafeConnection);

impl db::sqlez::domain::Domain for CommentsDb {
    const NAME: &str = stringify!(CommentsDb);

    // Historical migrations are immutable — once a row has run against a
    // user's database, the string can never change. Schema renames live in
    // ALTER migrations appended at the end.
    const MIGRATIONS: &[&str] = &[
        sql! (
            CREATE TABLE comment_threads (
                workspace_id INTEGER NOT NULL,
                thread_id TEXT NOT NULL,
                path TEXT NOT NULL,
                start_row INTEGER NOT NULL,
                start_column INTEGER NOT NULL,
                end_row INTEGER NOT NULL,
                end_column INTEGER NOT NULL,
                fingerprint TEXT NOT NULL,
                kind INTEGER NOT NULL,
                status INTEGER NOT NULL,
                collapsed INTEGER NOT NULL,
                PRIMARY KEY (workspace_id, thread_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                    ON UPDATE CASCADE
            ) STRICT;

            CREATE TABLE comment_nodes (
                workspace_id INTEGER NOT NULL,
                thread_id TEXT NOT NULL,
                node_id TEXT NOT NULL,
                parent_id TEXT,
                author_kind INTEGER NOT NULL,
                author_login TEXT,
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                remote_id INTEGER,
                PRIMARY KEY (workspace_id, thread_id, node_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                    ON UPDATE CASCADE
            ) STRICT;
        ),
        sql! (
            ALTER TABLE comment_nodes ADD COLUMN kind INTEGER NOT NULL DEFAULT 0;
        ),
        // Rename tables and columns to the universal-model vocabulary
        // (conversation / comment / in_reply_to). The Rust types and SQL
        // queries below use the post-rename names.
        sql! (
            ALTER TABLE comment_threads RENAME TO conversations;
            ALTER TABLE comment_nodes RENAME TO comments;
            ALTER TABLE conversations RENAME COLUMN thread_id TO conversation_id;
            ALTER TABLE comments RENAME COLUMN thread_id TO conversation_id;
            ALTER TABLE comments RENAME COLUMN node_id TO comment_id;
            ALTER TABLE comments RENAME COLUMN parent_id TO in_reply_to;
        ),
    ];
}

db::static_connection!(CommentsDb, [WorkspaceDb]);

impl CommentsDb {
    query! {
        pub fn load_conversations(workspace_id: WorkspaceId) -> Result<Vec<DbConversationRow>> {
            SELECT conversation_id, path, start_row, start_column, end_row, end_column,
                   fingerprint, kind, status, collapsed
            FROM conversations
            WHERE workspace_id = ?
        }
    }

    query! {
        pub fn load_comments(workspace_id: WorkspaceId) -> Result<Vec<DbCommentRow>> {
            SELECT conversation_id, comment_id, in_reply_to, author_kind, author_login,
                   body, created_at, remote_id, kind
            FROM comments
            WHERE workspace_id = ?
        }
    }

    /// Atomically replaces every persisted conversation and comment for the
    /// workspace. Comment volume per workspace is small, so a full rewrite
    /// keeps the store and the database trivially consistent.
    pub async fn replace_all(
        &self,
        workspace_id: WorkspaceId,
        conversations: Vec<DbConversationRow>,
        comments: Vec<DbCommentRow>,
    ) -> Result<()> {
        self.write(move |conn| {
            conn.exec_bound(sql!(
                DELETE FROM conversations WHERE workspace_id = ?;
            ))?(workspace_id)?;
            conn.exec_bound(sql!(
                DELETE FROM comments WHERE workspace_id = ?;
            ))?(workspace_id)?;

            for conversation in conversations {
                conn.exec_bound(sql!(
                    INSERT OR REPLACE INTO conversations
                        (workspace_id, conversation_id, path, start_row, start_column,
                         end_row, end_column, fingerprint, kind, status, collapsed)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);
                ))?((workspace_id, conversation))?;
            }

            for comment in comments {
                conn.exec_bound(sql!(
                    INSERT OR REPLACE INTO comments
                        (workspace_id, conversation_id, comment_id, in_reply_to, author_kind,
                         author_login, body, created_at, remote_id, kind)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?);
                ))?((workspace_id, comment))?;
            }

            Ok(())
        })
        .await
    }
}
