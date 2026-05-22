//! Workspace-local sqlite persistence for inline code comments.
//!
//! Mirrors the breakpoint / `file_folds` persistence pattern: comment threads
//! and their nodes are stored keyed by `workspace_id`, with positions kept as
//! row/column pairs plus a text fingerprint so they can be re-anchored after
//! the file changes.

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

/// One persisted comment thread (without its `workspace_id`, which is implied
/// by the query).
#[derive(Clone, Debug)]
pub struct DbThreadRow {
    pub thread_id: String,
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

/// One persisted comment node (a single message within a thread's tree).
#[derive(Clone, Debug)]
pub struct DbNodeRow {
    pub thread_id: String,
    pub node_id: String,
    pub parent_id: Option<String>,
    pub author_kind: i64,
    pub author_login: Option<String>,
    pub body: String,
    pub created_at: i64,
    pub remote_id: Option<i64>,
}

impl StaticColumnCount for DbThreadRow {
    fn column_count() -> usize {
        10
    }
}

impl Bind for DbThreadRow {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        let next = statement.bind(&self.thread_id, start_index)?;
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

impl Column for DbThreadRow {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (thread_id, next) = Column::column(statement, start_index)?;
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
                thread_id,
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

impl StaticColumnCount for DbNodeRow {
    fn column_count() -> usize {
        8
    }
}

impl Bind for DbNodeRow {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        let next = statement.bind(&self.thread_id, start_index)?;
        let next = statement.bind(&self.node_id, next)?;
        let next = statement.bind(&self.parent_id, next)?;
        let next = statement.bind(&self.author_kind, next)?;
        let next = statement.bind(&self.author_login, next)?;
        let next = statement.bind(&self.body, next)?;
        let next = statement.bind(&self.created_at, next)?;
        let next = statement.bind(&self.remote_id, next)?;
        Ok(next)
    }
}

impl Column for DbNodeRow {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (thread_id, next) = Column::column(statement, start_index)?;
        let (node_id, next) = Column::column(statement, next)?;
        let (parent_id, next) = Column::column(statement, next)?;
        let (author_kind, next) = Column::column(statement, next)?;
        let (author_login, next) = Column::column(statement, next)?;
        let (body, next) = Column::column(statement, next)?;
        let (created_at, next) = Column::column(statement, next)?;
        let (remote_id, next) = Column::column(statement, next)?;
        Ok((
            Self {
                thread_id,
                node_id,
                parent_id,
                author_kind,
                author_login,
                body,
                created_at,
                remote_id,
            },
            next,
        ))
    }
}

pub struct CommentsDb(db::sqlez::thread_safe_connection::ThreadSafeConnection);

impl db::sqlez::domain::Domain for CommentsDb {
    const NAME: &str = stringify!(CommentsDb);

    const MIGRATIONS: &[&str] = &[sql! (
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
    )];
}

db::static_connection!(CommentsDb, [WorkspaceDb]);

impl CommentsDb {
    query! {
        pub fn load_threads(workspace_id: WorkspaceId) -> Result<Vec<DbThreadRow>> {
            SELECT thread_id, path, start_row, start_column, end_row, end_column,
                   fingerprint, kind, status, collapsed
            FROM comment_threads
            WHERE workspace_id = ?
        }
    }

    query! {
        pub fn load_nodes(workspace_id: WorkspaceId) -> Result<Vec<DbNodeRow>> {
            SELECT thread_id, node_id, parent_id, author_kind, author_login,
                   body, created_at, remote_id
            FROM comment_nodes
            WHERE workspace_id = ?
        }
    }

    /// Atomically replaces every persisted thread and node for the workspace.
    /// Comment volume per workspace is small, so a full rewrite keeps the
    /// store and the database trivially consistent.
    pub async fn replace_all(
        &self,
        workspace_id: WorkspaceId,
        threads: Vec<DbThreadRow>,
        nodes: Vec<DbNodeRow>,
    ) -> Result<()> {
        self.write(move |conn| {
            conn.exec_bound(sql!(
                DELETE FROM comment_threads WHERE workspace_id = ?;
            ))?(workspace_id)?;
            conn.exec_bound(sql!(
                DELETE FROM comment_nodes WHERE workspace_id = ?;
            ))?(workspace_id)?;

            for thread in threads {
                conn.exec_bound(sql!(
                    INSERT OR REPLACE INTO comment_threads
                        (workspace_id, thread_id, path, start_row, start_column,
                         end_row, end_column, fingerprint, kind, status, collapsed)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);
                ))?((workspace_id, thread))?;
            }

            for node in nodes {
                conn.exec_bound(sql!(
                    INSERT OR REPLACE INTO comment_nodes
                        (workspace_id, thread_id, node_id, parent_id, author_kind,
                         author_login, body, created_at, remote_id)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?);
                ))?((workspace_id, node))?;
            }

            Ok(())
        })
        .await
    }
}
