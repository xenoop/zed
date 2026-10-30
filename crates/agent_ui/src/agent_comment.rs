use std::ops::RangeInclusive;

use editor::ReviewCommentForAgent;
use util::markdown::MarkdownCodeBlock;

/// What a comment is attached to. Comments on code and on the conversation share one
/// shape, so the agent always receives them the same way.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CommentTarget {
    Code {
        path: String,
        /// Zero-based rows in the file.
        rows: RangeInclusive<u32>,
        code: String,
    },
    Conversation {
        excerpt: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentComment {
    pub target: CommentTarget,
    pub note: String,
}

impl AgentComment {
    pub fn from_review_comment(comment: ReviewCommentForAgent) -> Self {
        Self {
            target: CommentTarget::Code {
                path: comment.path.as_unix_str().to_string(),
                rows: comment.rows,
                code: comment.code,
            },
            note: comment.comment,
        }
    }

    pub fn on_conversation(excerpt: impl Into<String>) -> Self {
        Self {
            target: CommentTarget::Conversation {
                excerpt: excerpt.into(),
            },
            note: String::new(),
        }
    }

    pub fn to_prompt_text(&self) -> String {
        let mut text = match &self.target {
            CommentTarget::Code { path, rows, code } => {
                let first_line = rows.start() + 1;
                let last_line = rows.end() + 1;
                let lines = if first_line == last_line {
                    format!("line {first_line}")
                } else {
                    format!("lines {first_line}-{last_line}")
                };
                format!(
                    "Comment on `{path}` ({lines}):\n{}",
                    MarkdownCodeBlock {
                        tag: "",
                        text: code,
                    }
                )
            }
            CommentTarget::Conversation { excerpt } => {
                let quoted = excerpt
                    .trim()
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("Comment on your earlier reply:\n{quoted}\n\n")
            }
        };
        text.push_str(self.note.trim());
        text
    }
}

pub(crate) fn comments_to_prompt_text(comments: &[AgentComment]) -> String {
    comments
        .iter()
        .map(AgentComment::to_prompt_text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_and_conversation_comments_share_one_shape() {
        let code = AgentComment {
            target: CommentTarget::Code {
                path: "src/main.rs".into(),
                rows: 1..=2,
                code: "    todo!()\n    // ```".into(),
            },
            note: "Implement this".into(),
        };
        assert_eq!(
            code.to_prompt_text(),
            "Comment on `src/main.rs` (lines 2-3):\n````\n    todo!()\n    // ```\n````\nImplement this"
        );

        let conversation = AgentComment {
            note: "Why not a hash map?".into(),
            ..AgentComment::on_conversation("I used a vector.\nIt keeps order.")
        };
        assert_eq!(
            conversation.to_prompt_text(),
            "Comment on your earlier reply:\n> I used a vector.\n> It keeps order.\n\nWhy not a hash map?"
        );

        assert_eq!(
            comments_to_prompt_text(&[code.clone(), conversation.clone()]),
            format!(
                "{}\n\n{}",
                code.to_prompt_text(),
                conversation.to_prompt_text()
            )
        );
    }
}
