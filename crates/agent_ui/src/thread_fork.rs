//! Thread forks: a copy of a thread's working tree in its own git worktree,
//! whose changes can later be merged back into the tree it was forked from.
//!
//! Git runs through the project's shell, so this works for remote projects
//! too. Merging never moves the parent's `HEAD` or touches its index: both
//! trees are captured as commits, merged with `git merge-tree`, and the
//! result is applied to the parent's files as a patch.

use anyhow::{Context as _, Result, anyhow};
use db::kvp::KeyValueStore;
use gpui::{App, AsyncApp, Entity};
use project::Project;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

/// The most forks of one working tree that may exist at a time. Each fork is
/// a full checkout, so an unbounded number fills the disk and slows git.
pub(crate) const MAX_LIVE_FORKS: usize = 8;

const FORKS_KEY: &str = "agent_thread_forks";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ForkKind {
    Fork,
    Review,
    Land,
}

/// A fork of a working tree, as remembered between sessions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ForkRecord {
    /// The fork's worktree directory.
    pub fork_path: String,
    /// The directory of the working tree it was forked from.
    pub parent_path: String,
    /// Commit capturing the parent's working tree when the fork was made.
    pub base_sha: String,
    /// Commit capturing the fork's working tree at the last merge into the
    /// parent: later merges only carry what changed since.
    pub merged_sha: Option<String>,
    pub kind: ForkKind,
    /// Title of the thread that was forked, for menus.
    pub parent_title: Option<String>,
    /// The verdict of a review fork, once submitted.
    #[serde(default)]
    pub verdict: Option<ReviewVerdict>,
    /// The thread that was forked, so messages back reach it rather than
    /// whichever thread its workspace shows.
    #[serde(default)]
    pub parent_thread_id: Option<crate::thread_metadata_store::ThreadId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ReviewVerdict {
    Approved,
    ChangesRequested,
}

impl ForkRecord {
    /// A short name for menus: the last component of the fork's directory
    /// that is not the repository name repeated.
    pub fn display_name(&self) -> String {
        let mut components = self
            .fork_path
            .rsplit(['/', '\\'])
            .filter(|part| !part.is_empty());
        let last = components.next().unwrap_or(&self.fork_path);
        match components.next() {
            Some(previous) => format!("{previous}/{last}"),
            None => last.to_string(),
        }
    }
}

/// How a fork's working tree relates to the one it was made from, for views
/// that list threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkLink {
    pub fork_path: std::path::PathBuf,
    pub parent_path: std::path::PathBuf,
    pub parent_thread_id: Option<crate::thread_metadata_store::ThreadId>,
}

/// Every known fork, with its parent.
pub fn fork_links(cx: &App) -> Vec<ForkLink> {
    load_records(cx)
        .into_iter()
        .map(|record| ForkLink {
            fork_path: record.fork_path.into(),
            parent_path: record.parent_path.into(),
            parent_thread_id: record.parent_thread_id,
        })
        .collect()
}

/// What a thread list needs to know about one of its thread rows to place
/// forks under their parents.
pub struct ForkListRow {
    pub thread_id: crate::thread_metadata_store::ThreadId,
    /// The folders the thread works in.
    pub folder_paths: Vec<std::path::PathBuf>,
    /// Set when the thread works in a fork.
    pub fork_parent: Option<ForkLink>,
}

/// Moves each row of a thread that works in a fork to just below the thread
/// it was forked from: the recorded thread, or failing that the first
/// listed thread of the parent working tree. A fork whose parent is not in
/// the list stays where it is. `describe` returns `None` for rows that are
/// not threads.
pub fn nest_forks_under_parents<T>(
    mut rows: Vec<T>,
    describe: impl Fn(&T) -> Option<ForkListRow>,
) -> Vec<T> {
    let forks: Vec<(crate::thread_metadata_store::ThreadId, ForkLink)> = rows
        .iter()
        .filter_map(|row| {
            let row = describe(row)?;
            Some((row.thread_id, row.fork_parent?))
        })
        .collect();
    for (fork_id, link) in forks {
        let position = |rows: &[T], matches: &dyn Fn(&ForkListRow) -> bool| {
            rows.iter()
                .position(|row| describe(row).is_some_and(|row| matches(&row)))
        };
        let parent_index = position(&rows, &|row| {
            row.thread_id != fork_id && Some(row.thread_id) == link.parent_thread_id
        })
        .or_else(|| {
            position(&rows, &|row| {
                row.thread_id != fork_id && row.folder_paths.contains(&link.parent_path)
            })
        });
        let fork_index = position(&rows, &|row| row.thread_id == fork_id);
        let (Some(parent_index), Some(fork_index)) = (parent_index, fork_index) else {
            continue;
        };
        let fork_row = rows.remove(fork_index);
        let parent_index = if fork_index < parent_index {
            parent_index - 1
        } else {
            parent_index
        };
        // After the forks already placed under this parent, so siblings
        // keep their order.
        let mut insert_at = parent_index + 1;
        while rows.get(insert_at).is_some_and(|row| {
            describe(row)
                .and_then(|row| row.fork_parent)
                .is_some_and(|other| other.parent_path == link.parent_path)
        }) {
            insert_at += 1;
        }
        rows.insert(insert_at, fork_row);
    }
    rows
}

/// The fork records, held in memory once read: the database write is
/// asynchronous, so reading back from it right after a change could miss it.
#[derive(Default)]
struct ForkRecords(Vec<ForkRecord>);

impl gpui::Global for ForkRecords {}

pub(crate) fn load_records(cx: &App) -> Vec<ForkRecord> {
    if let Some(records) = cx.try_global::<ForkRecords>() {
        return records.0.clone();
    }
    KeyValueStore::global(cx)
        .read_kvp(FORKS_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
        .unwrap_or_default()
}

pub(crate) fn save_records(records: Vec<ForkRecord>, cx: &mut App) {
    cx.set_global(ForkRecords(records.clone()));
    let store = KeyValueStore::global(cx);
    db::write_and_log(cx, move || async move {
        store
            .write_kvp(FORKS_KEY.to_string(), serde_json::to_string(&records)?)
            .await
    });
}

/// Adds or replaces the record for a fork directory.
pub(crate) fn upsert_record(record: ForkRecord, cx: &mut App) {
    let mut records = load_records(cx);
    records.retain(|existing| existing.fork_path != record.fork_path);
    records.push(record);
    save_records(records, cx);
}

pub(crate) fn record_for_fork(fork_path: &str, cx: &App) -> Option<ForkRecord> {
    load_records(cx)
        .into_iter()
        .find(|record| record.fork_path == fork_path)
}

pub(crate) fn records_for_parent(parent_path: &str, cx: &App) -> Vec<ForkRecord> {
    load_records(cx)
        .into_iter()
        .filter(|record| record.parent_path == parent_path)
        .collect()
}

/// Starts the first message of a fork: the thread view shows such messages
/// collapsed to one line.
pub(crate) const FORK_CONTEXT_PREFIX: &str = "This thread is a fork of";

/// How much of the parent's transcript a fork receives, counted from its end.
const MAX_TRANSCRIPT_BYTES: usize = 60_000;

/// The first message of a fork. Agents cannot import another thread's
/// history, so the parent's transcript is passed as text.
pub(crate) fn fork_context_message(parent_title: Option<&str>, transcript: &str) -> String {
    let title = parent_title.unwrap_or("an earlier thread");
    let mut start = transcript.len().saturating_sub(MAX_TRANSCRIPT_BYTES);
    while !transcript.is_char_boundary(start) {
        start += 1;
    }
    let truncated = if start > 0 {
        "(earlier part omitted)\n"
    } else {
        ""
    };
    format!(
        "{FORK_CONTEXT_PREFIX} \"{title}\". Its conversation so far is below, for context. \
         You are working in a separate git worktree that started as a copy of that \
         thread's working tree, including uncommitted changes; what you change here \
         reaches the original only when I merge it. Do not act on the conversation \
         yet: reply with one short line saying you are ready.\n\n\
         <conversation>\n{truncated}{}\n</conversation>\n",
        &transcript[start..]
    )
}

pub(crate) fn fork_thread_title(kind: &ForkKind, parent_title: Option<&str>) -> String {
    let label = match kind {
        ForkKind::Fork => "Fork",
        ForkKind::Review => "Review",
        ForkKind::Land => "Land",
    };
    match parent_title {
        Some(title) => format!("{label}: {title}"),
        None => label.to_string(),
    }
}

/// The message a fork's thread starts with, by kind.
pub(crate) fn first_message(
    kind: &ForkKind,
    parent_title: Option<&str>,
    transcript: &str,
) -> String {
    let context = fork_context_message(parent_title, transcript);
    match kind {
        ForkKind::Fork | ForkKind::Land => context,
        ForkKind::Review => context.replace(
            "Do not act on the conversation yet: reply with one short line saying you are ready.",
            REVIEW_INSTRUCTIONS,
        ),
    }
}

const REVIEW_INSTRUCTIONS: &str = "You are the reviewer of that thread's work. The work is \
     everything between the repository's default branch and this worktree's HEAD commit, \
     which is a snapshot that includes the uncommitted changes. Do not change any files. \
     Write a change guide: an ordered walkthrough of the change for a reader who has not \
     seen it, one step per logical change, each naming the files and lines involved and \
     saying what changed and why it matters, with anything risky, unclear or untested \
     called out. I will then comment and give a verdict.";

/// Starts a message that carries a review's verdict to the reviewed thread.
pub(crate) const VERDICT_PREFIX: &str = "Review verdict:";
/// Starts a message passed on from another thread.
pub(crate) const THREAD_MESSAGE_PREFIX: &str = "Message from thread";
/// Starts the first message of a thread that lands changes.
pub(crate) const LAND_PREFIX: &str = "Land the changes of thread";

const MAX_FORWARDED_BYTES: usize = 8_000;

fn clipped(text: &str) -> &str {
    let mut end = text.len().min(MAX_FORWARDED_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The message a review's verdict reaches the reviewed thread with: the
/// verdict, what the user added, and the reviewer's last findings.
pub(crate) fn verdict_message(
    verdict: ReviewVerdict,
    note: &str,
    reviewer_findings: &str,
) -> String {
    let (label, instruction) = match verdict {
        ReviewVerdict::Approved => (
            "approved",
            "No changes are required. Take note of the findings below, if any.",
        ),
        ReviewVerdict::ChangesRequested => (
            "changes requested",
            "Address what is asked below, then say what you changed.",
        ),
    };
    let mut message = format!("{VERDICT_PREFIX} {label}. {instruction}\n");
    if !note.trim().is_empty() {
        message.push_str(&format!("\nFrom me:\n{}\n", note.trim()));
    }
    if !reviewer_findings.trim().is_empty() {
        message.push_str(&format!(
            "\nThe reviewer's findings:\n{}\n",
            clipped(reviewer_findings.trim())
        ));
    }
    message
}

/// The message another thread's reply is passed on with.
pub(crate) fn thread_message(sender_title: Option<&str>, text: &str) -> String {
    let title = sender_title.unwrap_or("another thread");
    format!(
        "{THREAD_MESSAGE_PREFIX} \"{title}\":\n\n{}\n",
        clipped(text.trim())
    )
}

/// The first message of a thread that lands another thread's changes.
/// `steps` is the project's own landing procedure, when it has one.
pub(crate) fn land_message(parent_title: Option<&str>, steps: Option<&str>) -> String {
    let title = parent_title.unwrap_or("this working tree");
    let steps = steps
        .map(str::trim)
        .filter(|steps| !steps.is_empty())
        .unwrap_or(
            "1. Make sure the work is committed in sensible commits on the current branch.\n\
         2. Bring the branch up to date with the default branch (rebase unless the \
         project merges).\n\
         3. Run the project's checks and tests; stop if they fail.\n\
         4. Push the branch, and open or update its pull request.",
        );
    format!(
        "{LAND_PREFIX} \"{title}\". You are in the same working tree as that thread. \
         Follow these steps in order. Stop at the first step that fails and do not force \
         anything (no force-push, no skipped checks).\n\n{steps}\n\n\
         Finish with a short report: what landed, what did not, and what I need to do.\n"
    )
}

/// The project's own landing procedure, from `.zed/land.md` in the working
/// tree, if it has one.
pub(crate) async fn landing_steps(
    project: &Entity<Project>,
    directory: &str,
    cx: &mut AsyncApp,
) -> Option<String> {
    let script = format!("cat {}/.zed/land.md 2>/dev/null || true", quote(directory));
    run_script(project, script, cx)
        .await
        .log_err()
        .filter(|steps| !steps.trim().is_empty())
}

/// The one-line form of a message delivered from another thread, a verdict
/// or a landing request, for the collapsed row in the transcript.
pub(crate) fn delivered_message_summary(text: &str) -> Option<String> {
    if let Some(rest) = text.strip_prefix(VERDICT_PREFIX) {
        let verdict = rest.trim_start().split('.').next().unwrap_or_default();
        return Some(format!("Review verdict: {verdict}"));
    }
    let quoted_title = |rest: &str| {
        rest.trim_start()
            .strip_prefix('"')
            .and_then(|rest| rest.split_once('"'))
            .map(|(title, _)| title.to_string())
    };
    if let Some(rest) = text.strip_prefix(THREAD_MESSAGE_PREFIX) {
        return Some(match quoted_title(rest) {
            Some(title) => format!("Message from thread \"{title}\""),
            None => "Message from another thread".to_string(),
        });
    }
    if let Some(rest) = text.strip_prefix(LAND_PREFIX) {
        return Some(match quoted_title(rest) {
            Some(title) => format!("Landing the changes of \"{title}\""),
            None => "Landing changes".to_string(),
        });
    }
    None
}

/// What a merge did, in a sentence for a notification.
pub(crate) fn merge_summary(fork_name: &str, outcome: &MergeOutcome) -> String {
    match outcome {
        MergeOutcome::NothingToMerge => {
            format!("{fork_name} has no changes that are not already merged")
        }
        MergeOutcome::Merged {
            changed_files,
            conflicted_files,
        } => {
            let files = match changed_files.len() {
                1 => "1 file".to_string(),
                count => format!("{count} files"),
            };
            if conflicted_files.is_empty() {
                format!("Merged {files} from {fork_name}")
            } else {
                format!(
                    "Merged {files} from {fork_name}. Resolve the conflict markers in: {}",
                    conflicted_files.join(", ")
                )
            }
        }
    }
}

/// The panel action a message such as `/fork` stands for.
pub(crate) fn slash_command_action(text: &str) -> Option<Box<dyn gpui::Action>> {
    match text {
        "/fork" => Some(Box::new(zed_actions::agent::ForkThread)),
        "/merge" => Some(Box::new(zed_actions::agent::MergeIntoParent)),
        "/review" => Some(Box::new(zed_actions::agent::ReviewThread)),
        "/land" => Some(Box::new(zed_actions::agent::LandChanges)),
        "/send-to-parent" => Some(Box::new(zed_actions::agent::SendToParentThread)),
        _ => None,
    }
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Runs a POSIX shell script in the project's environment (local or remote)
/// and returns its standard output, trimmed.
async fn run_script(
    project: &Entity<Project>,
    script: String,
    cx: &mut AsyncApp,
) -> Result<String> {
    let command = project.update(cx, |project, cx| project.exec_in_shell(script, cx));
    let output = command.await?.output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("{}\n{}", stderr.trim(), stdout).context("git failed"));
    }
    Ok(stdout)
}

/// Captures the whole working tree of `directory` (staged, unstaged and
/// untracked files) as a commit, without changing `HEAD` or the index.
pub(crate) async fn snapshot(
    project: &Entity<Project>,
    directory: &str,
    cx: &mut AsyncApp,
) -> Result<String> {
    let script = format!(
        "cd {directory} && \
         index=\"$(git rev-parse --absolute-git-dir)/zed-fork-index.$$\" && \
         cp \"$(git rev-parse --absolute-git-dir)/index\" \"$index\" 2>/dev/null; \
         tree=$(GIT_INDEX_FILE=\"$index\" git add --all && GIT_INDEX_FILE=\"$index\" git write-tree); \
         status=$?; rm -f \"$index\"; [ $status -eq 0 ] || exit $status; \
         git -c user.name=Zed -c user.email=zed@localhost \
         commit-tree \"$tree\" -p HEAD -m 'Zed fork snapshot'",
        directory = quote(directory),
    );
    let sha = run_script(project, script, cx)
        .await
        .context("could not snapshot the working tree")?;
    anyhow::ensure!(
        sha.len() >= 40 && sha.chars().all(|c| c.is_ascii_hexdigit()),
        "unexpected snapshot id {sha:?}"
    );
    Ok(sha)
}

/// The directories of the repository's linked worktrees, after pruning
/// entries whose directory is gone.
pub(crate) async fn live_worktree_paths(
    project: &Entity<Project>,
    directory: &str,
    cx: &mut AsyncApp,
) -> Result<Vec<String>> {
    let script = format!(
        "cd {} && git worktree prune && git worktree list --porcelain",
        quote(directory)
    );
    let listing = run_script(project, script, cx).await?;
    Ok(listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|path| path.to_string())
        .collect())
}

/// Drops records whose fork directory is no longer a worktree of the
/// repository, and returns the live forks of `parent_path`.
pub(crate) async fn live_forks(
    project: &Entity<Project>,
    parent_path: &str,
    cx: &mut AsyncApp,
) -> Result<Vec<ForkRecord>> {
    let worktrees = live_worktree_paths(project, parent_path, cx).await?;
    let records = cx.update(|cx| load_records(cx));
    let (live, stale): (Vec<_>, Vec<_>) = records.into_iter().partition(|record| {
        record.parent_path != parent_path || worktrees.contains(&record.fork_path)
    });
    if !stale.is_empty() {
        let live = live.clone();
        cx.update(|cx| save_records(live, cx));
    }
    Ok(live
        .into_iter()
        .filter(|record| record.parent_path == parent_path)
        .collect())
}

/// Refuses a new fork once [`MAX_LIVE_FORKS`] of the working tree exist.
pub(crate) async fn ensure_room_for_fork(
    project: &Entity<Project>,
    parent_path: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let live = live_forks(project, parent_path, cx).await?;
    anyhow::ensure!(
        live.len() < MAX_LIVE_FORKS,
        "This working tree already has {} forks. Merge and archive some of them before forking again.",
        live.len()
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MergeOutcome {
    /// The fork has no changes that the parent has not already received.
    NothingToMerge,
    Merged {
        /// Files the merge changed in the parent.
        changed_files: Vec<String>,
        /// Files left with conflict markers for the user to resolve.
        conflicted_files: Vec<String>,
    },
}

/// Merges what the fork changed since it was made (or since its last merge)
/// into the parent's working tree, and returns the updated record.
///
/// This is a three-way merge of the two working trees, so edits made in the
/// parent meanwhile are kept; where both changed the same lines, the file
/// gets conflict markers. The parent's `HEAD` and index stay as they are.
pub(crate) async fn merge_into_parent(
    project: &Entity<Project>,
    mut record: ForkRecord,
    cx: &mut AsyncApp,
) -> Result<(ForkRecord, MergeOutcome)> {
    let fork_sha = snapshot(project, &record.fork_path, cx).await?;
    let merge_base = record.merged_sha.clone().unwrap_or(record.base_sha.clone());
    let parent = quote(&record.parent_path);

    let unchanged = run_script(
        project,
        format!(
            "cd {parent} && [ \"$(git rev-parse {fork_sha}^{{tree}})\" = \"$(git rev-parse {merge_base}^{{tree}})\" ] && echo same || echo different"
        ),
        cx,
    )
    .await?;
    if unchanged == "same" {
        return Ok((record, MergeOutcome::NothingToMerge));
    }

    let parent_sha = snapshot(project, &record.parent_path, cx).await?;
    // `merge-tree` exits with 1 when there are conflicts; the tree it prints
    // then holds the conflict markers.
    let script = format!(
        "cd {parent} && \
         merged=$(git merge-tree --write-tree --merge-base={merge_base} {parent_sha} {fork_sha}); \
         status=$?; [ $status -le 1 ] || exit $status; \
         tree=$(printf '%s\\n' \"$merged\" | head -n 1) && \
         patch=$(mktemp) && \
         git diff --binary {parent_sha} \"$tree\" > \"$patch\" && \
         git diff --name-only {parent_sha} \"$tree\" && \
         echo '--conflicts--' && \
         (git diff --name-only --diff-filter=U {parent_sha} \"$tree\"; \
          [ $status -eq 1 ] && printf '%s\\n' \"$merged\" | sed -n 's/^CONFLICT.* in \\(.*\\)$/\\1/p'; true) && \
         if [ -s \"$patch\" ]; then git apply --whitespace=nowarn \"$patch\"; fi; \
         applied=$?; rm -f \"$patch\"; exit $applied"
    );
    let output = run_script(project, script, cx)
        .await
        .context("could not merge the fork into its parent")?;
    let (changed, conflicted) = output
        .split_once("--conflicts--")
        .unwrap_or((output.as_str(), ""));
    let lines = |text: &str| -> Vec<String> {
        let mut lines: Vec<String> = text
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();
        lines.sort();
        lines.dedup();
        lines
    };

    record.merged_sha = Some(fork_sha);
    let updated = record.clone();
    cx.update(|cx| upsert_record(updated, cx));
    Ok((
        record,
        MergeOutcome::Merged {
            changed_files: lines(changed),
            conflicted_files: lines(conflicted),
        },
    ))
}

/// Whether the fork holds changes its parent has not received.
pub(crate) async fn has_unmerged_changes(
    project: &Entity<Project>,
    record: &ForkRecord,
    cx: &mut AsyncApp,
) -> Result<bool> {
    let fork_sha = snapshot(project, &record.fork_path, cx).await?;
    let merge_base = record.merged_sha.clone().unwrap_or(record.base_sha.clone());
    let result = run_script(
        project,
        format!(
            "cd {} && [ \"$(git rev-parse {fork_sha}^{{tree}})\" = \"$(git rev-parse {merge_base}^{{tree}})\" ] && echo same || echo different",
            quote(&record.fork_path)
        ),
        cx,
    )
    .await?;
    Ok(result != "same")
}

/// Removes a fork's worktree and its record. A fork with changes that were
/// never merged is kept, and an error says so.
pub(crate) async fn remove_fork(
    project: &Entity<Project>,
    record: &ForkRecord,
    cx: &mut AsyncApp,
) -> Result<()> {
    anyhow::ensure!(
        !has_unmerged_changes(project, record, cx).await?,
        "The fork at {} has changes that were not merged into its parent, so it was kept.",
        record.fork_path
    );
    run_script(
        project,
        format!(
            "cd {} && git worktree remove --force {} && git worktree prune",
            quote(&record.parent_path),
            quote(&record.fork_path)
        ),
        cx,
    )
    .await
    .context("could not remove the fork's worktree")?;
    let fork_path = record.fork_path.clone();
    cx.update(|cx| {
        let mut records = load_records(cx);
        records.retain(|existing| existing.fork_path != fork_path);
        save_records(records, cx);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use settings::SettingsStore;
    use std::path::Path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        cx.executor().allow_parking();
    }

    #[allow(clippy::disallowed_methods)]
    fn git(directory: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(directory)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        parent: std::path::PathBuf,
        project: Entity<Project>,
    }

    impl Fixture {
        async fn new(cx: &mut TestAppContext) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let parent = directory.path().canonicalize().unwrap().join("repo");
            std::fs::create_dir_all(&parent).unwrap();
            std::fs::write(parent.join("a.txt"), "one\ntwo\nthree\n").unwrap();
            std::fs::write(parent.join("b.txt"), "alpha\n").unwrap();
            git(&parent, &["init", "-b", "main"]);
            git(&parent, &["add", "."]);
            git(&parent, &["commit", "-m", "initial"]);
            let project =
                Project::test(fs::RealFs::new(None, cx.executor()), [parent.as_path()], cx).await;
            Self {
                _directory: directory,
                parent,
                project,
            }
        }

        fn parent_path(&self) -> String {
            self.parent.to_string_lossy().to_string()
        }

        /// Forks the parent's working tree the way the agent panel does:
        /// a detached worktree at a snapshot of it.
        async fn fork(&self, name: &str, cx: &mut TestAppContext) -> ForkRecord {
            let parent_path = self.parent_path();
            let mut async_cx = cx.to_async();
            ensure_room_for_fork(&self.project, &parent_path, &mut async_cx)
                .await
                .unwrap();
            let base_sha = snapshot(&self.project, &parent_path, &mut async_cx)
                .await
                .unwrap();
            let fork = self.parent.parent().unwrap().join(name);
            git(
                &self.parent,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    fork.to_str().unwrap(),
                    &base_sha,
                ],
            );
            let record = ForkRecord {
                fork_path: fork.to_string_lossy().to_string(),
                parent_path,
                base_sha,
                merged_sha: None,
                kind: ForkKind::Fork,
                parent_title: None,
                verdict: None,
                parent_thread_id: None,
            };
            cx.update(|cx| upsert_record(record.clone(), cx));
            cx.run_until_parked();
            record
        }
    }

    #[gpui::test]
    async fn test_fork_carries_uncommitted_work_and_merges_back(cx: &mut TestAppContext) {
        init_test(cx);
        let fixture = Fixture::new(cx).await;
        let parent = fixture.parent.clone();
        // Uncommitted and untracked work in the parent travels to the fork.
        std::fs::write(parent.join("b.txt"), "alpha edited\n").unwrap();
        std::fs::write(parent.join("untracked.txt"), "draft\n").unwrap();
        let head = git(&parent, &["rev-parse", "HEAD"]);

        let record = fixture.fork("fork-one", cx).await;
        let fork = Path::new(&record.fork_path).to_path_buf();
        assert_eq!(
            std::fs::read_to_string(fork.join("b.txt")).unwrap(),
            "alpha edited\n"
        );
        assert_eq!(
            std::fs::read_to_string(fork.join("untracked.txt")).unwrap(),
            "draft\n"
        );

        // The fork and the parent then change different things.
        std::fs::write(fork.join("a.txt"), "one\ntwo\nthree\nfour from fork\n").unwrap();
        std::fs::write(fork.join("new.txt"), "made in fork\n").unwrap();
        std::fs::remove_file(fork.join("untracked.txt")).unwrap();
        std::fs::write(parent.join("a.txt"), "zero from parent\none\ntwo\nthree\n").unwrap();

        let mut async_cx = cx.to_async();
        let (record, outcome) = merge_into_parent(&fixture.project, record, &mut async_cx)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Merged {
                changed_files: vec![
                    "a.txt".to_string(),
                    "new.txt".to_string(),
                    "untracked.txt".to_string()
                ],
                conflicted_files: Vec::new(),
            }
        );
        assert_eq!(
            std::fs::read_to_string(parent.join("a.txt")).unwrap(),
            "zero from parent\none\ntwo\nthree\nfour from fork\n"
        );
        assert_eq!(
            std::fs::read_to_string(parent.join("new.txt")).unwrap(),
            "made in fork\n"
        );
        assert!(!parent.join("untracked.txt").exists());
        assert_eq!(
            std::fs::read_to_string(parent.join("b.txt")).unwrap(),
            "alpha edited\n"
        );
        // The parent's history and index are untouched.
        assert_eq!(git(&parent, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&parent, &["diff", "--cached", "--name-only"]), "");

        // Merging again carries nothing; a later change is merged alone.
        let (record, outcome) = merge_into_parent(&fixture.project, record, &mut async_cx)
            .await
            .unwrap();
        assert_eq!(outcome, MergeOutcome::NothingToMerge);
        std::fs::write(fork.join("new.txt"), "made in fork\nand extended\n").unwrap();
        let (_, outcome) = merge_into_parent(&fixture.project, record, &mut async_cx)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Merged {
                changed_files: vec!["new.txt".to_string()],
                conflicted_files: Vec::new(),
            }
        );
        assert_eq!(
            std::fs::read_to_string(parent.join("new.txt")).unwrap(),
            "made in fork\nand extended\n"
        );
    }

    #[test]
    fn test_fork_messages() {
        let message = fork_context_message(Some("Fix the parser"), "## User\nhello\n");
        assert!(message.starts_with(FORK_CONTEXT_PREFIX));
        assert!(message.contains("\"Fix the parser\""));
        assert!(message.contains("<conversation>\n## User\nhello\n\n</conversation>"));

        // A long transcript keeps its end.
        let long = format!("{}THE END", "x".repeat(MAX_TRANSCRIPT_BYTES * 2));
        let message = fork_context_message(None, &long);
        assert!(message.contains("(earlier part omitted)"));
        assert!(message.contains("THE END"));
        assert!(message.len() < MAX_TRANSCRIPT_BYTES + 2_000);

        assert_eq!(
            fork_thread_title(&ForkKind::Fork, Some("Fix the parser")),
            "Fork: Fix the parser"
        );
        assert!(slash_command_action("/fork").is_some());
        assert!(slash_command_action("/merge").is_some());
        assert!(slash_command_action("/fork please").is_none());

        assert_eq!(
            merge_summary(
                "worktrees/fork",
                &MergeOutcome::Merged {
                    changed_files: vec!["a".into(), "b".into()],
                    conflicted_files: vec!["b".into()],
                }
            ),
            "Merged 2 files from worktrees/fork. Resolve the conflict markers in: b"
        );
    }

    #[test]
    fn test_review_land_and_thread_messages() {
        let review = first_message(&ForkKind::Review, Some("Fix the parser"), "hello");
        assert!(review.starts_with(FORK_CONTEXT_PREFIX));
        assert!(review.contains("Write a change guide"));
        assert!(!review.contains("reply with one short line"));

        let verdict = verdict_message(
            ReviewVerdict::ChangesRequested,
            "Rename the helper.\n",
            "1. The parser drops the last token.",
        );
        assert_eq!(
            verdict,
            "Review verdict: changes requested. Address what is asked below, then say what you \
             changed.\n\nFrom me:\nRename the helper.\n\nThe reviewer's findings:\n\
             1. The parser drops the last token.\n"
        );
        assert_eq!(
            delivered_message_summary(&verdict).as_deref(),
            Some("Review verdict: changes requested")
        );
        let approved = verdict_message(ReviewVerdict::Approved, "", "");
        assert_eq!(
            delivered_message_summary(&approved).as_deref(),
            Some("Review verdict: approved")
        );

        let forwarded = thread_message(Some("Review: Fix the parser"), "All good.");
        assert_eq!(
            forwarded,
            "Message from thread \"Review: Fix the parser\":\n\nAll good.\n"
        );
        assert_eq!(
            delivered_message_summary(&forwarded).as_deref(),
            Some("Message from thread \"Review: Fix the parser\"")
        );

        let land = land_message(Some("Fix the parser"), None);
        assert!(land.contains("Push the branch"));
        assert!(land.contains("no force-push"));
        assert_eq!(
            delivered_message_summary(&land).as_deref(),
            Some("Landing the changes of \"Fix the parser\"")
        );
        let custom = land_message(None, Some("1. make ship\n"));
        assert!(custom.contains("1. make ship") && !custom.contains("Push the branch"));

        assert!(delivered_message_summary("An ordinary message").is_none());
        for command in ["/review", "/land", "/send-to-parent"] {
            assert!(slash_command_action(command).is_some(), "{command}");
        }
    }

    #[test]
    fn test_forks_are_listed_under_their_parent_thread() {
        use crate::thread_metadata_store::ThreadId;
        use std::path::PathBuf;

        struct Row {
            title: &'static str,
            thread_id: ThreadId,
            path: &'static str,
            fork_parent: Option<ForkLink>,
        }
        let row = |title, path, fork_parent| Row {
            title,
            thread_id: ThreadId::new(),
            path,
            fork_parent,
        };
        let link = |fork_path: &str, parent_path: &str, parent_thread_id| ForkLink {
            fork_path: PathBuf::from(fork_path),
            parent_path: PathBuf::from(parent_path),
            parent_thread_id,
        };
        let older_parent = row("older parent", "/repo", None);
        // As a thread list sorts them: most recent first.
        let rows = vec![
            row(
                "fork two",
                "/worktrees/two",
                Some(link("/worktrees/two", "/repo", None)),
            ),
            row("unrelated", "/other", None),
            row("recent parent", "/repo", None),
            row(
                "fork one",
                "/worktrees/one",
                Some(link(
                    "/worktrees/one",
                    "/repo",
                    Some(older_parent.thread_id),
                )),
            ),
            older_parent,
            row(
                "orphan fork",
                "/worktrees/orphan",
                Some(link("/worktrees/orphan", "/gone", None)),
            ),
        ];
        let titles: Vec<&str> = nest_forks_under_parents(rows, |row| {
            Some(ForkListRow {
                thread_id: row.thread_id,
                folder_paths: vec![PathBuf::from(row.path)],
                fork_parent: row.fork_parent.clone(),
            })
        })
        .iter()
        .map(|row| row.title)
        .collect();
        assert_eq!(
            titles,
            vec![
                "unrelated",
                // Without a recorded thread, the fork follows the first
                // listed thread of its parent working tree.
                "recent parent",
                "fork two",
                // With one, it follows that thread.
                "older parent",
                "fork one",
                // A fork whose parent is not listed stays in place.
                "orphan fork",
            ]
        );
    }

    #[gpui::test]
    async fn test_conflicting_merge_leaves_markers(cx: &mut TestAppContext) {
        init_test(cx);
        let fixture = Fixture::new(cx).await;
        let parent = fixture.parent.clone();
        let record = fixture.fork("fork-conflict", cx).await;
        let fork = Path::new(&record.fork_path).to_path_buf();
        std::fs::write(fork.join("b.txt"), "from fork\n").unwrap();
        std::fs::write(parent.join("b.txt"), "from parent\n").unwrap();

        let mut async_cx = cx.to_async();
        let (_, outcome) = merge_into_parent(&fixture.project, record, &mut async_cx)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Merged {
                changed_files: vec!["b.txt".to_string()],
                conflicted_files: vec!["b.txt".to_string()],
            }
        );
        let merged = std::fs::read_to_string(parent.join("b.txt")).unwrap();
        assert!(merged.contains("<<<<<<<"), "{merged}");
        assert!(merged.contains("from fork") && merged.contains("from parent"));
    }

    #[gpui::test]
    async fn test_fork_cleanup_is_safe_and_capped(cx: &mut TestAppContext) {
        init_test(cx);
        let fixture = Fixture::new(cx).await;
        let parent_path = fixture.parent_path();
        let record = fixture.fork("fork-keep", cx).await;
        let fork = Path::new(&record.fork_path).to_path_buf();
        let mut async_cx = cx.to_async();

        // A fork with work that was never merged is not removed.
        std::fs::write(fork.join("b.txt"), "unmerged work\n").unwrap();
        let error = remove_fork(&fixture.project, &record, &mut async_cx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not merged"), "{error:#}");
        assert!(fork.exists());

        // Once merged, it is removed along with its record and git's entry.
        let (record, _) = merge_into_parent(&fixture.project, record, &mut async_cx)
            .await
            .unwrap();
        remove_fork(&fixture.project, &record, &mut async_cx)
            .await
            .unwrap();
        cx.run_until_parked();
        assert!(!fork.exists());
        assert!(
            cx.update(|cx| records_for_parent(&parent_path, cx))
                .is_empty()
        );
        assert!(!git(&fixture.parent, &["worktree", "list"]).contains("fork-keep"));

        // A fork deleted behind Zed's back is forgotten and pruned from git.
        let gone = fixture.fork("fork-gone", cx).await;
        std::fs::remove_dir_all(&gone.fork_path).unwrap();
        let live = live_forks(&fixture.project, &parent_path, &mut async_cx)
            .await
            .unwrap();
        assert!(live.is_empty());
        assert!(!git(&fixture.parent, &["worktree", "list"]).contains("fork-gone"));

        // Forking stops at the cap.
        for index in 0..MAX_LIVE_FORKS {
            fixture.fork(&format!("fork-{index}"), cx).await;
        }
        let error = ensure_room_for_fork(&fixture.project, &parent_path, &mut async_cx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already has"), "{error:#}");
    }
}
