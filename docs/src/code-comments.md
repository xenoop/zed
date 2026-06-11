# Code Comments

Zed can attach persistent, threaded comments to lines of code, in the spirit of a pull-request review thread. Comments are rendered inline in the editor (and in the Project Diff multibuffer), survive restarts, follow the commented line as the file changes, and can be sent to the AI agent for follow-up.

## Adding comments

Place the cursor on a line (or select a range) and run {#action code_comments::AddComment} from the command palette. A composer appears below the line; submitting it creates the thread's first comment. From there you can:

- **Reply** to any comment, forming a nested thread.
- **Edit** your own comments.
- **Resolve** or **Reopen** a thread. Resolved threads collapse into a compact one-line strip.
- **Collapse** a thread (or a single comment's subtree) to reclaim editor space.
- **Delete** a thread from its root card.

If the commented line is edited or moved, Zed re-anchors the comment using the text of the original line. When the line can no longer be found, the thread is flagged **Outdated** instead of being silently dropped.

Comments are stored locally, per project, in Zed's workspace database. They are not written into your source files.

## Comment kinds

Each comment can be classified as one of three kinds (click the kind chip to cycle):

- **Comment** — a plain note, never sent anywhere automatically.
- **Question** — something you want answered.
- **Task** — something that should be acted on.

Any comment can be sent to the agent panel's active thread with its **Send to agent** button; the agent's answer is appended to the thread as a reply. {#action code_comments::SendTasksToAgent} delivers every open Task-kind thread to the agent in one batch.

## Settings

```json
{
  "code_comments": {
    "enabled": true,
    "show_in_editor": true,
    "sync": {
      "enabled": false,
      "provider": "custom",
      "review_unit": "auto",
      "auto_pull_on_open": true,
      "custom": {
        "detect_command": [],
        "fetch_command": []
      }
    }
  }
}
```

- `enabled`: turns the feature off entirely.
- `show_in_editor`: hides the inline cards without disabling the feature.
- `sync`: read-only synchronization with an external code-review system, described below.

## Syncing with a code review system

When `sync.enabled` is `true`, Zed resolves which review unit (pull request, merge request, changelist, …) the current worktree corresponds to and pulls that review's comment threads into the editor. Threads fetched from the review system are scoped to that review unit: they appear while you are working on it and disappear when you switch away (for example, to another branch). Local threads created while a review unit is active are scoped to it as well.

The review unit is named by the `review_unit` setting, or detected automatically when it is `"auto"`. Repeated pulls update previously fetched threads in place; replies you have written locally are preserved.

### The `custom` provider

The built-in `custom` provider drives sync with two user-configured commands, so any review system with a CLI can plug in without modifying Zed:

- `detect_command` runs in the repository root and prints the active review unit's id on stdout (or nothing if there isn't one). For GitHub, for example: `["gh", "pr", "view", "--json", "number", "--jq", ".number"]`.
- `fetch_command` runs in the repository root with the review unit id appended as its final argument, and prints the review's comment threads as JSON on stdout.

The fetch output is a JSON array of threads:

```json
[
  {
    "path": "src/main.rs",
    "start_line": 10,
    "end_line": 12,
    "line_text": "fn main() {",
    "revision": "1abc234",
    "resolved": false,
    "comments": [
      { "id": 1, "author": "alice", "body": "Why is this safe?", "created_at": 1700000000 },
      { "id": 2, "in_reply_to": 1, "author": "bob", "body": "See the check above." }
    ]
  }
]
```

- `path` is relative to the worktree root, with unix-style separators; line numbers are 1-based.
- `line_text` (optional) is the text of the commented line, used to re-anchor the thread after local edits.
- `revision` (optional) records which commit the position refers to.
- Each comment's `id` must be stable across fetches — it is how Zed matches comments between pulls. `in_reply_to` refers to the `id` of the parent comment.

Pulls run when a project opens and whenever the repository's HEAD changes (such as a branch switch), as long as `auto_pull_on_open` is `true`. Syncing is currently read-only: comments you create in Zed stay local and are not pushed to the review system.
