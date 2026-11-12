use std::{
    cell::RefCell,
    hash::{DefaultHasher, Hasher},
    path::Path,
    sync::Arc,
};

use collections::HashMap;
use editor::Editor;
use git::repository::RepoPath;
use gpui::{App, Entity, Global, SharedString, actions};
use language::BufferId;
use multi_buffer::MultiBuffer;
use project::git_store::diff_buffer_list::{DiffBase, DiffBufferList};

actions!(
    git,
    [
        /// Marks the file under the cursor in the Changes view as seen (folding it), or
        /// clears the mark.
        ToggleFileSeen,
        /// Marks every file in the Changes view as seen.
        MarkAllFilesSeen,
        /// Moves to the next file in the Changes view.
        GoToNextChangedFile,
        /// Moves to the previous file in the Changes view.
        GoToPreviousChangedFile,
    ]
);

/// Which comparison a set of marks belongs to: a file seen against the branch base is
/// not seen against the last turn.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SeenScope {
    work_directory: Arc<Path>,
    comparison: SharedString,
}

#[derive(Clone, PartialEq)]
struct FingerprintKey {
    buffer_version: clock::Global,
    base: Option<(BufferId, clock::Global)>,
}

/// Seen marks for Changes views. Each mark stores a fingerprint of the file's diff
/// when it was marked, so a later edit to either side shows the file as unseen again.
#[derive(Default)]
pub(crate) struct SeenFiles {
    marks: HashMap<SeenScope, HashMap<RepoPath, u64>>,
    fingerprints: RefCell<HashMap<BufferId, (FingerprintKey, u64)>>,
}

impl Global for SeenFiles {}

struct SeenFile {
    scope: SeenScope,
    repo_path: RepoPath,
    fingerprint: u64,
}

fn scope(list: &DiffBufferList, cx: &App) -> Option<SeenScope> {
    let comparison = match list.diff_base() {
        DiffBase::Merge { base_ref } => base_ref.clone(),
        DiffBase::Snapshot { commit, .. } => commit.clone(),
        DiffBase::Head => "HEAD".into(),
        DiffBase::Index => "index".into(),
        DiffBase::Staged => "staged".into(),
    };
    let work_directory = list.repo()?.read(cx).work_directory_abs_path.clone();
    Some(SeenScope {
        work_directory,
        comparison,
    })
}

fn hash_chunks<'a>(hasher: &mut DefaultHasher, chunks: impl Iterator<Item = &'a str>) {
    // Hashing the bytes chunk by chunk keeps the result independent of how the rope
    // happens to be split.
    for chunk in chunks {
        hasher.write(chunk.as_bytes());
    }
}

fn seen_file(
    list: &DiffBufferList,
    multibuffer: &MultiBuffer,
    buffer_id: BufferId,
    cx: &App,
) -> Option<SeenFile> {
    let scope = scope(list, cx)?;
    let buffer = multibuffer.buffer(buffer_id)?;
    let buffer = buffer.read(cx);
    let file = buffer.file()?;
    let repo_path = list
        .repo()?
        .read(cx)
        .project_path_to_repo_path(&(file.worktree_id(cx), file.path().clone()).into(), cx)?;

    let diff = multibuffer.diff_for(buffer_id);
    let base = diff.as_ref().and_then(|diff| {
        let diff = diff.read(cx);
        diff.base_text_exists()
            .then(|| diff.base_text_buffer().read(cx))
    });
    let key = FingerprintKey {
        buffer_version: buffer.version(),
        base: base.map(|base| (base.remote_id(), base.version())),
    };

    let seen_files = cx.try_global::<SeenFiles>();
    let cached = seen_files.and_then(|seen_files| {
        let fingerprints = seen_files.fingerprints.borrow();
        let (cached_key, fingerprint) = fingerprints.get(&buffer_id)?;
        (cached_key == &key).then_some(*fingerprint)
    });
    let fingerprint = cached.unwrap_or_else(|| {
        let mut hasher = DefaultHasher::new();
        hash_chunks(&mut hasher, buffer.as_rope().chunks());
        hasher.write_u8(0xff);
        match base {
            Some(base) => {
                hasher.write_u8(1);
                hash_chunks(&mut hasher, base.as_rope().chunks());
            }
            None => hasher.write_u8(0),
        }
        let fingerprint = hasher.finish();
        if let Some(seen_files) = seen_files {
            seen_files
                .fingerprints
                .borrow_mut()
                .insert(buffer_id, (key, fingerprint));
        }
        fingerprint
    });

    Some(SeenFile {
        scope,
        repo_path,
        fingerprint,
    })
}

fn is_marked(seen_files: &SeenFiles, file: &SeenFile) -> bool {
    seen_files
        .marks
        .get(&file.scope)
        .and_then(|marks| marks.get(&file.repo_path))
        .is_some_and(|fingerprint| *fingerprint == file.fingerprint)
}

fn has_mark(seen_files: &SeenFiles, scope: &SeenScope, repo_path: &RepoPath) -> bool {
    seen_files
        .marks
        .get(scope)
        .is_some_and(|marks| marks.contains_key(repo_path))
}

pub(crate) fn is_seen(
    list: &DiffBufferList,
    multibuffer: &MultiBuffer,
    buffer_id: BufferId,
    cx: &App,
) -> bool {
    let Some(seen_files) = cx.try_global::<SeenFiles>() else {
        return false;
    };
    let Some(scope) = scope(list, cx) else {
        return false;
    };
    if seen_files
        .marks
        .get(&scope)
        .is_none_or(|marks| marks.is_empty())
    {
        return false;
    }
    seen_file(list, multibuffer, buffer_id, cx).is_some_and(|file| is_marked(seen_files, &file))
}

/// The files of the Changes view in display order.
pub(crate) fn ordered_buffer_ids(multibuffer: &MultiBuffer, cx: &App) -> Vec<BufferId> {
    let snapshot = multibuffer.snapshot(cx);
    let mut buffer_ids: Vec<BufferId> = Vec::new();
    for excerpt in snapshot.excerpts() {
        let buffer_id = excerpt.context.start.buffer_id;
        if buffer_ids.last() != Some(&buffer_id) {
            buffer_ids.push(buffer_id);
        }
    }
    buffer_ids
}

/// How many files of the Changes view are seen, out of how many.
pub(crate) fn progress(
    list: &DiffBufferList,
    multibuffer: &MultiBuffer,
    cx: &App,
) -> (usize, usize) {
    let buffer_ids = ordered_buffer_ids(multibuffer, cx);
    let total = buffer_ids.len();
    let seen = buffer_ids
        .into_iter()
        .filter(|buffer_id| is_seen(list, multibuffer, *buffer_id, cx))
        .count();
    (seen, total)
}

/// Marks or unmarks files, folding the ones marked seen and unfolding the others.
pub(crate) fn set_seen(
    list: &Entity<DiffBufferList>,
    editor: &Entity<Editor>,
    buffer_ids: &[BufferId],
    seen: bool,
    cx: &mut App,
) {
    let multibuffer = editor.read(cx).buffer().clone();
    let files = buffer_ids
        .iter()
        .filter_map(|buffer_id| {
            let file = seen_file(list.read(cx), multibuffer.read(cx), *buffer_id, cx)?;
            Some((*buffer_id, file))
        })
        .collect::<Vec<_>>();
    if files.is_empty() {
        return;
    }
    let seen_files = cx.default_global::<SeenFiles>();
    for (_, file) in &files {
        let marks = seen_files.marks.entry(file.scope.clone()).or_default();
        if seen {
            marks.insert(file.repo_path.clone(), file.fingerprint);
        } else {
            marks.remove(&file.repo_path);
        }
    }
    let buffer_ids = files.into_iter().map(|(buffer_id, _)| buffer_id);
    editor.update(cx, |editor, cx| {
        if seen {
            editor.fold_buffers(buffer_ids, cx);
        } else {
            for buffer_id in buffer_ids {
                editor.unfold_buffer(buffer_id, cx);
            }
        }
    });
}

/// Drops the marks of files whose diff changed since they were marked, and unfolds
/// them, so new edits are not hidden behind a stale "seen".
pub(crate) fn clear_stale_marks(
    list: &Entity<DiffBufferList>,
    editor: &Entity<Editor>,
    cx: &mut App,
) {
    let Some(seen_files) = cx.try_global::<SeenFiles>() else {
        return;
    };
    let Some(scope) = scope(list.read(cx), cx) else {
        return;
    };
    if seen_files
        .marks
        .get(&scope)
        .is_none_or(|marks| marks.is_empty())
    {
        return;
    }
    let multibuffer = editor.read(cx).buffer().clone();
    let stale = ordered_buffer_ids(multibuffer.read(cx), cx)
        .into_iter()
        .filter(|buffer_id| {
            seen_file(list.read(cx), multibuffer.read(cx), *buffer_id, cx).is_some_and(|file| {
                has_mark(seen_files, &file.scope, &file.repo_path) && !is_marked(seen_files, &file)
            })
        })
        .collect::<Vec<_>>();
    if !stale.is_empty() {
        set_seen(list, editor, &stale, false, cx);
    }
}

/// The file after (or before) the one holding the cursor.
pub(crate) fn adjacent_buffer_id(editor: &Editor, forward: bool, cx: &App) -> Option<BufferId> {
    let buffer_ids = ordered_buffer_ids(editor.buffer().read(cx), cx);
    let current = editor
        .active_buffer(cx)
        .map(|buffer| buffer.read(cx).remote_id());
    let index = current.and_then(|current| buffer_ids.iter().position(|id| *id == current));
    let target = match (index, forward) {
        (Some(index), true) => index + 1,
        (Some(index), false) => index.checked_sub(1)?,
        (None, true) => 0,
        (None, false) => buffer_ids.len().checked_sub(1)?,
    };
    buffer_ids.get(target).copied()
}
