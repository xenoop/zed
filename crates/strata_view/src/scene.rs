//! A step at one moment of its animation, flattened into what the painter
//! needs. Built from `strata_core::timeline::sample`, so the static before
//! and after views are simply the first and last moments.

use gpui::SharedString;
#[cfg(test)]
use strata_core::layout::Layout;
use strata_core::layout::{self, Point, Rect};
#[cfg(test)]
use strata_core::timeline::step_timeline;
use strata_core::timeline::{Frame, Placement, Side, Timeline, frame_path_in, sample_in};
use strata_spec::{Element, ElementKind, Relationship, RelationshipKind, Spec, Step};

pub use strata_core::timeline::Mark;

/// Which end of a step's change is shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// The previous step, marked with what is about to change.
    Before,
    /// This step, marked with what just changed.
    After,
}

/// How an item is drawn at this moment.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Look {
    pub alpha: f32,
    pub mark: Option<Mark>,
    /// Drawn as a dashed outline only: where an element was, or will be.
    pub ghost: bool,
    /// How brightly the item glows as it lands, 0 to 1.
    pub glow: f32,
}

#[cfg(test)]
impl Look {
    const PLAIN: Look = Look {
        alpha: 1.0,
        mark: None,
        ghost: false,
        glow: 0.0,
    };
    const DIMMED: Look = Look {
        alpha: strata_core::timeline::timing::UNCHANGED_OPACITY,
        mark: None,
        ghost: false,
        glow: 0.0,
    };
}

/// An element's text, for cross-fading changed content.
#[derive(Clone)]
pub struct NodeText {
    pub title: SharedString,
    pub technology: Option<SharedString>,
    pub description: Option<SharedString>,
}

pub struct BoundaryItem {
    pub id: SharedString,
    pub rect: Rect,
    pub depth: u32,
    pub title: SharedString,
    /// The previous title, while it cross-fades into `title`.
    pub previous_title: Option<SharedString>,
    pub content_mix: f32,
    pub look: Look,
}

pub struct NodeItem {
    pub id: SharedString,
    pub rect: Rect,
    pub kind: ElementKind,
    pub text: NodeText,
    /// The previous text, while it cross-fades into `text`.
    pub previous_text: Option<NodeText>,
    pub content_mix: f32,
    pub look: Look,
}

pub struct LabelItem {
    pub rect: Rect,
    pub text: Option<SharedString>,
    pub technology: Option<SharedString>,
}

pub struct EdgeItem {
    pub from: SharedString,
    pub to: SharedString,
    /// Cubic Bézier control points in layout coordinates.
    pub points: Vec<Point>,
    pub tip: Point,
    pub dashed: bool,
    pub hollow_head: bool,
    pub label: Option<LabelItem>,
    pub bounds: Rect,
    /// How much of the path is drawn, from its source.
    pub drawn: f32,
    pub look: Look,
}

pub struct Scene {
    pub size: layout::Size,
    pub boundaries: Vec<BoundaryItem>,
    pub edges: Vec<EdgeItem>,
    pub nodes: Vec<NodeItem>,
}

fn bounds_of(points: &[Point], extra: Option<Point>) -> Rect {
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for point in points.iter().chain(extra.iter()) {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    if min_x > max_x {
        return Rect::default();
    }
    Rect {
        x: min_x,
        y: min_y,
        w: max_x - min_x,
        h: max_y - min_y,
    }
}

fn trimmed(value: Option<&str>) -> Option<SharedString> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| SharedString::from(value.to_string()))
}

fn node_text(element: &Element) -> NodeText {
    NodeText {
        title: element.label.clone().into(),
        technology: trimmed(element.technology.as_deref()),
        description: trimmed(element.description.as_deref()),
    }
}

fn find<'a>(step: &'a Step, id: &str) -> Option<&'a Element> {
    step.elements.iter().find(|element| element.id == id)
}

fn find_relationship<'a>(step: &'a Step, from: &str, to: &str) -> Option<&'a Relationship> {
    step.relationships
        .iter()
        .find(|relationship| relationship.from == from && relationship.to == to)
}

impl Scene {
    /// Nothing yet, until the first frame is sampled.
    pub fn empty() -> Scene {
        Scene {
            size: layout::Size::default(),
            boundaries: Vec::new(),
            edges: Vec::new(),
            nodes: Vec::new(),
        }
    }

    /// The step's before (`t = 0`) or after (`t = end`) view.
    #[cfg(test)]
    pub fn build(spec: &Spec, step_index: usize, phase: Phase, layout: &Layout) -> Option<Scene> {
        let timeline = step_timeline(spec, step_index);
        let t = match phase {
            Phase::Before => 0.0,
            Phase::After => timeline.duration,
        };
        Scene::at(spec, step_index, &timeline, t, layout)
    }

    /// The step `t` seconds into its animation, on a stable layout.
    #[cfg(test)]
    pub fn at(
        spec: &Spec,
        step_index: usize,
        timeline: &Timeline,
        t: f32,
        layout: &Layout,
    ) -> Option<Scene> {
        Scene::at_placed(spec, step_index, timeline, t, Placement::stable(layout))
    }

    /// The step `t` seconds into its animation, between the layouts of its
    /// two ends.
    pub fn at_placed(
        spec: &Spec,
        step_index: usize,
        timeline: &Timeline,
        t: f32,
        placement: Placement,
    ) -> Option<Scene> {
        let next = spec.steps.get(step_index)?;
        let previous = step_index
            .checked_sub(1)
            .and_then(|index| spec.steps.get(index));
        let frame: Frame = sample_in(timeline, placement, previous, next, t);
        let step_of = |side: Side| match side {
            Side::Next => Some(next),
            Side::Previous => previous,
        };

        let mut boundaries = Vec::new();
        let mut nodes = Vec::new();
        for item in &frame.elements {
            let Some(step) = step_of(item.side) else {
                continue;
            };
            let Some(element) = find(step, &item.id) else {
                continue;
            };
            let before = previous.and_then(|previous| find(previous, &item.id));
            let look = Look {
                alpha: item.opacity,
                mark: item.mark,
                ghost: item.ghost,
                glow: item.glow,
            };
            let layout = placement.of(item.side);
            if layout.is_boundary(step, element) {
                let previous_title = before
                    .filter(|before| item.content_mix < 1.0 && before.label != element.label)
                    .map(|before| SharedString::from(before.label.to_uppercase()));
                boundaries.push(BoundaryItem {
                    id: element.id.clone().into(),
                    rect: item.rect,
                    depth: layout.boundary_depth(step, element).unwrap_or(1),
                    title: element.label.to_uppercase().into(),
                    previous_title,
                    content_mix: item.content_mix,
                    look,
                });
            } else {
                let previous_text = before
                    .filter(|_| item.content_mix < 1.0 && item.side == Side::Next)
                    .map(node_text);
                nodes.push(NodeItem {
                    id: element.id.clone().into(),
                    rect: item.rect,
                    kind: element.kind,
                    text: node_text(element),
                    previous_text,
                    content_mix: item.content_mix,
                    look,
                });
            }
        }
        // Outer boundaries first, so nested ones paint over them.
        boundaries.sort_by_key(|boundary| std::cmp::Reverse(boundary.depth));

        let mut edges = Vec::new();
        for item in &frame.relationships {
            let Some(step) = step_of(item.side) else {
                continue;
            };
            let Some(relationship) = find_relationship(step, &item.from, &item.to) else {
                continue;
            };
            let Some((points, arrow_tip)) = frame_path_in(placement, previous, next, item) else {
                continue;
            };
            let Some(&last) = points.last() else {
                continue;
            };
            let label = placement
                .of(item.side)
                .edge_label_rect(step, relationship)
                .and_then(|rect| {
                    let text = trimmed(relationship.display_label());
                    let technology = trimmed(relationship.technology.as_deref())
                        .map(|technology| SharedString::from(format!("[ {technology} ]")));
                    (text.is_some() || technology.is_some()).then_some(LabelItem {
                        rect,
                        text,
                        technology,
                    })
                })
                // A label waits for a morphing path to reach its new route.
                .filter(|_| !item.ghost && item.morph.is_none());
            let tip = arrow_tip.unwrap_or(last);
            edges.push(EdgeItem {
                from: relationship.from.clone().into(),
                to: relationship.to.clone().into(),
                bounds: bounds_of(&points, Some(tip)),
                points,
                tip,
                dashed: item.ghost || relationship.kind == RelationshipKind::Implements,
                hollow_head: relationship.kind == RelationshipKind::Implements,
                label,
                drawn: item.drawn,
                look: Look {
                    alpha: item.opacity,
                    mark: item.mark,
                    ghost: item.ghost,
                    glow: 0.0,
                },
            });
        }
        Some(Scene {
            size: layout::Size {
                w: placement.before.size.w.max(placement.after.size.w),
                h: placement.before.size.h.max(placement.after.size.h),
            },
            boundaries,
            edges,
            nodes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::diff::{ElementChange, diff_into};
    use strata_core::layout::{LayoutEngine, Measure, union_layout};

    fn example() -> (Spec, Layout) {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/synthetic-stack.json"
        );
        let spec = strata_spec::load(path).unwrap_or_else(|error| panic!("{error}"));
        let layout = union_layout(&spec, &Measure::estimated(&spec), LayoutEngine::default())
            .unwrap_or_else(|error| panic!("{error:#}"));
        (spec, layout)
    }

    fn marks(scene: &Scene, mark: Mark, ghost: bool) -> Vec<String> {
        let mut titles: Vec<String> = scene
            .nodes
            .iter()
            .filter(|node| node.look.mark == Some(mark) && node.look.ghost == ghost)
            .map(|node| node.text.title.to_string())
            .chain(
                scene
                    .boundaries
                    .iter()
                    .filter(|boundary| {
                        boundary.look.mark == Some(mark) && boundary.look.ghost == ghost
                    })
                    .map(|boundary| boundary.title.to_string()),
            )
            .collect();
        titles.sort();
        titles
    }

    #[test]
    fn the_first_step_is_plain_in_both_phases() {
        let (spec, layout) = example();
        for phase in [Phase::Before, Phase::After] {
            let scene =
                Scene::build(&spec, 0, phase, &layout).unwrap_or_else(|| panic!("no scene"));
            assert!(scene.nodes.iter().all(|node| node.look == Look::PLAIN));
            assert!(scene.edges.iter().all(|edge| edge.look == Look::PLAIN));
            assert_eq!(
                scene.nodes.len() + scene.boundaries.len(),
                spec.steps[0].elements.len()
            );
        }
    }

    #[test]
    fn phases_mark_the_same_changes_from_both_sides() {
        let (spec, layout) = example();
        for index in 1..spec.steps.len() {
            let diff = diff_into(&spec.steps, index);
            let before = Scene::build(&spec, index, Phase::Before, &layout)
                .unwrap_or_else(|| panic!("no scene"));
            let after = Scene::build(&spec, index, Phase::After, &layout)
                .unwrap_or_else(|| panic!("no scene"));
            let labels = |change: fn(&ElementChange) -> bool| {
                let mut labels: Vec<String> = diff
                    .elements
                    .iter()
                    .filter(|(_, value)| change(value))
                    .filter_map(|(id, _)| {
                        spec.steps[index]
                            .elements
                            .iter()
                            .chain(&spec.steps[index - 1].elements)
                            .find(|element| &element.id == id)
                    })
                    .map(|element| {
                        if layout.is_boundary(&spec.steps[index], element)
                            || layout.is_boundary(&spec.steps[index - 1], element)
                        {
                            element.label.to_uppercase()
                        } else {
                            element.label.clone()
                        }
                    })
                    .collect();
                labels.sort();
                labels
            };
            let added = labels(|change| *change == ElementChange::Added);
            let removed = labels(|change| *change == ElementChange::Removed);
            assert_eq!(marks(&after, Mark::Added, false), added, "step {index}");
            assert_eq!(marks(&before, Mark::Added, true), added, "step {index}");
            assert_eq!(
                marks(&before, Mark::Removed, false),
                removed,
                "step {index}"
            );
            assert_eq!(marks(&after, Mark::Removed, true), removed, "step {index}");
        }
    }

    #[test]
    fn a_moved_element_is_marked_at_its_new_home_and_ghosted_at_its_old_one() {
        let (spec, layout) = example();
        let (index, id) = (1..spec.steps.len())
            .find_map(|index| {
                diff_into(&spec.steps, index)
                    .elements
                    .iter()
                    .find(|(_, change)| matches!(change, ElementChange::Moved { .. }))
                    .map(|(id, _)| (index, id.clone()))
            })
            .unwrap_or_else(|| panic!("the example has no move"));
        let element = |step: &Step| {
            step.elements
                .iter()
                .find(|element| element.id == id)
                .and_then(|element| layout.element_rect(step, element))
                .unwrap_or_else(|| panic!("no rect"))
        };
        let new_home = element(&spec.steps[index]);
        let old_home = element(&spec.steps[index - 1]);
        assert_ne!(new_home, old_home);
        let after =
            Scene::build(&spec, index, Phase::After, &layout).unwrap_or_else(|| panic!("no scene"));
        let rects = |ghost: bool| -> Vec<Rect> {
            after
                .nodes
                .iter()
                .filter(|node| node.look.mark == Some(Mark::Changed) && node.look.ghost == ghost)
                .map(|node| node.rect)
                .chain(
                    after
                        .boundaries
                        .iter()
                        .filter(|item| {
                            item.look.mark == Some(Mark::Changed) && item.look.ghost == ghost
                        })
                        .map(|item| item.rect),
                )
                .collect()
        };
        assert!(rects(false).contains(&new_home));
        assert!(rects(true).contains(&old_home));
    }

    #[test]
    fn unchanged_items_dim_only_when_something_changed() {
        let (spec, layout) = example();
        let after = Scene::build(&spec, spec.steps.len() - 1, Phase::After, &layout)
            .unwrap_or_else(|| panic!("no scene"));
        assert!(after.nodes.iter().any(|node| node.look == Look::DIMMED));
        assert!(after.nodes.iter().any(|node| node.look.mark.is_some()));
    }
}
