mod diagram_theme;
mod painter;
mod scene;
mod strata_item;
mod strata_panel;

use gpui::{App, actions};
use workspace::Workspace;

pub use strata_item::{RELOAD_DEBOUNCE, SPEEDS, StrataView, StrataViewEvent};
pub use strata_panel::StrataPanel;

actions!(
    strata,
    [
        /// Opens a stack preview of the active editor's buffer, which updates as the buffer changes.
        OpenPreview,
        /// Opens a stack preview of the active editor's buffer in a pane beside it.
        OpenPreviewToTheSide,
        /// Prompts for a stack spec file, opens it in the project and previews it.
        OpenStack,
        /// Toggles focus on the stack panel.
        ToggleFocus,
        /// Plays the previous step of the stack.
        PreviousStep,
        /// Plays the next step of the stack.
        NextStep,
        /// Plays or pauses the current step.
        TogglePlayback,
        /// Moves the playback position of the current step back a little.
        ScrubBackward,
        /// Moves the playback position of the current step forward a little.
        ScrubForward,
        /// Selects the next slower playback speed.
        SlowDown,
        /// Selects the next faster playback speed.
        SpeedUp,
        /// Shows the stack as it was before the current step.
        ShowBefore,
        /// Shows the stack as it is after the current step.
        ShowAfter,
        /// Closes the details of the selected element.
        CloseDetails,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        StrataView::register(workspace, window, cx);
        StrataPanel::register(workspace, window, cx);
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
    use language::Buffer;
    use settings::SettingsStore;

    use crate::scene::Phase;
    use crate::{RELOAD_DEBOUNCE, StrataView};

    const STACK: &str = include_str!("../tests/fixtures/synthetic-stack.json");

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    fn open_preview<'a>(
        text: &str,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<Buffer>,
        Entity<StrataView>,
        &'a mut VisualTestContext,
    ) {
        init_test(cx);
        let buffer = cx.new(|cx| Buffer::local(text, cx));
        let (view, cx) = cx.add_window_view({
            let buffer = buffer.clone();
            move |window, cx| StrataView::new(buffer, window, cx)
        });
        cx.run_until_parked();
        (buffer, view, cx)
    }

    fn edit_and_settle(buffer: &Entity<Buffer>, text: &str, cx: &mut VisualTestContext) {
        buffer.update(cx, |buffer, cx| buffer.set_text(text, cx));
        cx.executor()
            .advance_clock(RELOAD_DEBOUNCE + Duration::from_millis(1));
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn opens_a_stack_at_its_last_step(cx: &mut TestAppContext) {
        let (_buffer, view, cx) = open_preview(STACK, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.spec().steps.len(), 4);
            assert!(view.is_ready());
            assert!(view.problems().is_empty());
            assert_eq!(view.step_index(), 3);
            assert_eq!(view.phase(), Some(Phase::After));
        });
    }

    #[gpui::test]
    async fn playing_a_step_reaches_its_end_state(cx: &mut TestAppContext) {
        let (_buffer, view, cx) = open_preview(STACK, cx);
        view.update(cx, |view, cx| view.play_step(1, cx));
        view.read_with(cx, |view, _| {
            assert_eq!(view.step_index(), 1);
            assert!(view.playback().playing);
            assert_eq!(view.phase(), Some(Phase::Before));
        });
        view.update(cx, |view, _| {
            let duration = view.duration();
            assert!(duration > 0.0);
            view.tick(duration / 2.0);
            assert_eq!(view.phase(), None);
            view.tick(duration);
        });
        view.read_with(cx, |view, _| {
            assert!(!view.playback().playing);
            assert_eq!(view.playback().t, view.duration());
            assert_eq!(view.phase(), Some(Phase::After));
        });
    }

    #[gpui::test]
    async fn an_invalid_edit_keeps_the_last_valid_stack(cx: &mut TestAppContext) {
        let (buffer, view, cx) = open_preview(STACK, cx);
        edit_and_settle(&buffer, "{ \"version\": ", cx);
        view.read_with(cx, |view, _| {
            assert!(!view.problems().is_empty());
            assert!(
                view.problems()
                    .iter()
                    .all(|problem| !problem.message.is_empty())
            );
            assert_eq!(view.spec().steps.len(), 4);
            assert!(view.is_ready());
        });

        edit_and_settle(&buffer, STACK, cx);
        view.read_with(cx, |view, _| assert!(view.problems().is_empty()));
    }

    #[gpui::test]
    async fn an_edit_keeps_the_selected_step_by_id(cx: &mut TestAppContext) {
        let (buffer, view, cx) = open_preview(STACK, cx);
        view.update(cx, |view, cx| view.go_to(1, Phase::After, cx));
        let selected_id = view.read_with(cx, |view, _| view.step().map(|step| step.id.clone()));
        assert_eq!(selected_id.as_deref(), Some("349"));

        let mut value: serde_json::Value =
            serde_json::from_str(STACK).expect("the fixture is valid JSON");
        value["steps"][3]["title"] = serde_json::Value::String("Renamed last step".into());
        let edited = serde_json::to_string_pretty(&value).expect("the value serializes");
        edit_and_settle(&buffer, &edited, cx);

        view.read_with(cx, |view, _| {
            assert!(view.problems().is_empty());
            assert_eq!(view.step().map(|step| step.id.as_str()), Some("349"));
            assert_eq!(view.spec().steps[3].title, "Renamed last step");
        });
    }
}
