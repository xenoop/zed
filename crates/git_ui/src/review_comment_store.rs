//! Where review comments are kept between sessions: one list per
//! repository, shared by the Changes view and by file tabs.

use std::{cell::Cell, rc::Rc, time::Duration};

use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent, SavedReviewComment};
use gpui::{App, Context, Entity, Global, Task, TaskExt as _, Window};
use project::git_store::Repository;
use util::ResultExt as _;

/// The comments by database key, as last read or written. The database
/// write is asynchronous, so reading it back right after a change could
/// miss the change.
#[derive(Default)]
struct StoredComments(HashMap<String, Vec<SavedReviewComment>>);

impl Global for StoredComments {}

/// The database key for a repository's review comments. Comments sit on
/// lines of the working tree, so every view of it shares them.
pub(crate) fn key_for_repository(repository: &Entity<Repository>, cx: &App) -> String {
    format!(
        "changes_view_comments:{}",
        repository.read(cx).work_directory_abs_path.display()
    )
}

pub(crate) fn load(key: &str, cx: &App) -> Vec<SavedReviewComment> {
    if let Some(comments) = cx
        .try_global::<StoredComments>()
        .and_then(|stored| stored.0.get(key))
    {
        return comments.clone();
    }
    KeyValueStore::global(cx)
        .read_kvp(key)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
        .unwrap_or_default()
}

/// Replaces the stored comments of `files` with `comments`, leaving the
/// comments of every other file as they are: a view only speaks for the
/// files it shows.
pub(crate) fn save_for_files(
    key: String,
    files: &HashSet<String>,
    comments: Vec<SavedReviewComment>,
    cx: &mut App,
) {
    let mut all = load(&key, cx);
    all.retain(|comment| !files.contains(&comment.file_path));
    all.extend(comments);
    cx.default_global::<StoredComments>()
        .0
        .insert(key.clone(), all.clone());
    let store = KeyValueStore::global(cx);
    db::write_and_log(cx, move || async move {
        if all.is_empty() {
            store.delete_kvp(key).await
        } else {
            store.write_kvp(key, serde_json::to_string(&all)?).await
        }
    });
}

/// The repository key and worktree-relative path of the file a plain file
/// editor shows.
fn file_editor_location(editor: &Editor, cx: &App) -> Option<(String, String)> {
    if !editor.mode().is_full() {
        return None;
    }
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let path = buffer.read(cx).file()?.path().as_unix_str().to_string();
    let (repository, _) = editor
        .project()?
        .read(cx)
        .git_store()
        .read(cx)
        .repository_and_path_for_buffer_id(buffer.read(cx).remote_id(), cx)?;
    Some((key_for_repository(&repository, cx), path))
}

/// Gives a file tab the saved comments of its file, and saves the comments
/// made in it. Editors that are not plain file editors are left alone.
pub(crate) fn register_file_editor(
    editor: &mut Editor,
    window: Option<&mut Window>,
    cx: &mut Context<Editor>,
) {
    let Some(window) = window else {
        return;
    };
    let is_file_editor = editor.mode().is_full()
        && editor.project().is_some()
        && editor
            .buffer()
            .read(cx)
            .as_singleton()
            .is_some_and(|buffer| buffer.read(cx).file().is_some());
    if !is_file_editor {
        return;
    }

    let restored = Rc::new(Cell::new(false));
    cx.subscribe(&cx.entity(), {
        let restored = restored.clone();
        move |editor, _, event: &EditorEvent, cx| {
            if !matches!(event, EditorEvent::ReviewCommentsChanged { .. }) {
                return;
            }
            let Some((key, path)) = file_editor_location(editor, cx) else {
                return;
            };
            let mut comments = editor.saved_review_comments(cx);
            if !restored.get() {
                // The saved comments of this file are not in the editor
                // yet: keep them next to the new ones.
                for stored in load(&key, cx) {
                    if stored.file_path == path && !comments.contains(&stored) {
                        comments.push(stored);
                    }
                }
            }
            save_for_files(key, &HashSet::from_iter([path]), comments, cx);
        }
    })
    .detach();

    // A tab restored at startup exists before its repository is known.
    let restore: Task<anyhow::Result<()>> = cx.spawn_in(window, async move |editor, cx| {
        for _ in 0..20 {
            let location = editor.read_with(cx, |editor, cx| file_editor_location(editor, cx))?;
            if let Some((key, path)) = location {
                editor.update_in(cx, |editor, window, cx| {
                    let comments: Vec<SavedReviewComment> = load(&key, cx)
                        .into_iter()
                        .filter(|comment| comment.file_path == path)
                        .collect();
                    restored.set(true);
                    if !comments.is_empty() {
                        editor.set_show_diff_review_button(true, cx);
                        editor.restore_review_comments(comments, window, cx);
                    }
                })?;
                break;
            }
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
        }
        Ok(())
    });
    restore.detach_and_log_err(cx);
}
