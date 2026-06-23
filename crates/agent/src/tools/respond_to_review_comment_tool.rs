use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema as acp;
use gpui::{App, Entity, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Responds to a single diff-review comment left by the user.
///
/// When the user sends diff-review comments to you, each comment has a numeric
/// `comment_id`. Call this tool once per comment you want to address, passing
/// that comment's `comment_id` and your `response`. You may answer any subset of
/// the comments — only call this for comments where you have substantive
/// feedback, and keep each response focused on that specific comment. Your
/// response is shown inline in the diff, nested under the user's comment.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RespondToReviewCommentToolInput {
    /// The numeric id of the review comment you are answering.
    pub comment_id: usize,
    /// Your response to that specific comment.
    pub response: String,
}

pub struct RespondToReviewCommentTool {
    project: Entity<Project>,
}

impl RespondToReviewCommentTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for RespondToReviewCommentTool {
    type Input = RespondToReviewCommentToolInput;
    type Output = String;

    const NAME: &'static str = "respond_to_review_comment";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> ui::SharedString {
        if let Ok(input) = input {
            format!("Respond to review comment {}", input.comment_id).into()
        } else {
            "Respond to review comment".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| e.to_string())?;
            let comment_id = input.comment_id;
            project.update(cx, |project, cx| {
                project.deliver_review_response(comment_id, input.response, cx);
            });
            Ok(format!("Recorded response to review comment {comment_id}."))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project, git_store::GitStoreEvent};
    use settings::SettingsStore;
    use std::rc::Rc;
    use std::cell::RefCell;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[gpui::test]
    async fn test_respond_to_review_comment_emits_event(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), serde_json::json!({})).await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        // Capture review-response events broadcast on the git store.
        let received: Rc<RefCell<Vec<(usize, String)>>> = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let git_store = project.read(cx).git_store().clone();
            let received = received.clone();
            cx.subscribe(&git_store, move |_, event, _| {
                if let GitStoreEvent::ReviewResponse {
                    comment_id,
                    response,
                } = event
                {
                    received.borrow_mut().push((*comment_id, response.clone()));
                }
            })
        });

        let tool = Arc::new(RespondToReviewCommentTool::new(project));
        let (event_stream, _event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(RespondToReviewCommentToolInput {
                        comment_id: 7,
                        response: "Looks good".to_string(),
                    }),
                    event_stream,
                    cx,
                )
            })
            .await;

        assert!(result.is_ok(), "tool should succeed: {result:?}");
        cx.run_until_parked();
        assert_eq!(
            received.borrow().as_slice(),
            &[(7, "Looks good".to_string())]
        );
    }
}
