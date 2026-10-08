//! Port of src/features/source-control/ui/BranchPicker.test.ts.
//!
//! The React test that checked the search and branch names use the sans
//! typeface read CSS classes; GPUI sets the font on the element, so it has
//! no counterpart here.

use gpui::{
    AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    Styled as _, TestAppContext, VisualTestContext, Window, div, px,
};

use super::support::{Recorded, setup};
use crate::git::{GitBranchEntry, GitBranches};
use crate::model::branches::create_row_label;
use crate::ui::branch_picker::BranchPicker;

fn branches(names: &[&str]) -> GitBranches {
    GitBranches {
        current: Some("main".into()),
        detached: false,
        branches: names
            .iter()
            .map(|name| GitBranchEntry {
                name: name.to_string(),
                current: *name == "main",
                remote: None,
            })
            .collect(),
    }
}

fn render_picker<'a>(
    cx: &'a mut TestAppContext,
    cwd: &str,
    list: GitBranches,
) -> (
    Entity<BranchPicker>,
    &'a mut VisualTestContext,
    super::support::Setup,
) {
    let setup = setup(cx, Recorded::default().hooks());
    setup.backend.set_branches(cwd, Ok(list));
    let scm = setup.scm.clone();
    let cwd = cwd.to_string();
    let (picker, cx) = cx.add_window_view(move |window, cx| {
        BranchPicker::new(scm, cwd, Some("main".into()), window, cx)
    });
    cx.run_until_parked();
    (picker, cx, setup)
}

/// The picker inside a box the size of a toolbar trigger, as the session
/// toolbar shows it.
struct Toolbar(Entity<BranchPicker>);

impl Render for Toolbar {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(px(120.)).h(px(24.)).child(self.0.clone())
    }
}

fn render_in_toolbar<'a>(
    cx: &'a mut TestAppContext,
    cwd: &str,
    list: GitBranches,
) -> (
    Entity<BranchPicker>,
    &'a mut VisualTestContext,
    super::support::Setup,
) {
    let setup = setup(cx, Recorded::default().hooks());
    setup.backend.set_branches(cwd, Ok(list));
    let scm = setup.scm.clone();
    let cwd = cwd.to_string();
    let (toolbar, cx) = cx.add_window_view(move |window, cx| {
        Toolbar(cx.new(|cx| BranchPicker::new(scm, cwd, Some("main".into()), window, cx)))
    });
    cx.run_until_parked();
    let picker = toolbar.read_with(cx, |toolbar, _| toolbar.0.clone());
    (picker, cx, setup)
}

fn type_query(picker: &Entity<BranchPicker>, text: &str, cx: &mut VisualTestContext) {
    let input = picker.read_with(cx, |picker, _| picker.query_input().clone());
    let text = text.to_string();
    input.update_in(cx, |state, window, cx| state.set_value(text, window, cx));
    cx.run_until_parked();
}

#[gpui::test]
fn autofocuses_the_branch_search_input_when_the_popover_opens(cx: &mut TestAppContext) {
    let (picker, cx, _setup) = render_picker(cx, "/repo", branches(&["main"]));
    picker.update_in(cx, |picker, window, cx| picker.toggle(window, cx));
    cx.run_until_parked();
    assert!(picker.read_with(cx, |picker, _| picker.is_open()));
    let input = picker.read_with(cx, |picker, _| picker.query_input().clone());
    let focused = cx.update(|window, cx| input.read(cx).focus_handle(cx).is_focused(window));
    assert!(focused);
}

#[gpui::test]
fn asks_for_a_branch_name_before_creating_from_the_fixed_action(cx: &mut TestAppContext) {
    let (picker, cx, setup) = render_picker(cx, "/repo", branches(&["main"]));
    picker.update_in(cx, |picker, window, cx| picker.toggle(window, cx));
    let name = picker
        .read_with(cx, |picker, cx| picker.create_name(cx))
        .unwrap();
    assert_eq!(create_row_label(&name), "New branch");

    picker.update_in(cx, |picker, window, cx| {
        picker.pick_create(name, window, cx)
    });
    cx.run_until_parked();
    assert!(setup.git.calls("git_create_branch").is_empty());
    let dialog = picker
        .read_with(cx, |picker, _| picker.creating_dialog().cloned())
        .unwrap();
    let input = dialog.read_with(cx, |dialog, _| dialog.name_input().clone());
    let focused = cx.update(|window, cx| input.read(cx).focus_handle(cx).is_focused(window));
    assert!(focused);

    // Submitting an empty name does nothing.
    dialog.update(cx, |dialog, cx| dialog.submit(cx));
    cx.run_until_parked();
    assert!(setup.git.calls("git_create_branch").is_empty());

    input.update_in(cx, |state, window, cx| {
        state.set_value("feature/picker", window, cx)
    });
    dialog.update(cx, |dialog, cx| dialog.submit(cx));
    cx.run_until_parked();
    assert_eq!(
        setup.git.calls("git_create_branch"),
        vec![vec![
            "/repo".to_string(),
            "feature/picker".to_string(),
            String::new()
        ]]
    );
}

#[gpui::test]
fn updates_the_branch_creation_row_with_the_entered_name(cx: &mut TestAppContext) {
    let (picker, cx, _setup) = render_picker(cx, "/repo", branches(&["main"]));
    picker.update_in(cx, |picker, window, cx| picker.toggle(window, cx));
    type_query(&picker, "feature/picker", cx);
    let name = picker
        .read_with(cx, |picker, cx| picker.create_name(cx))
        .unwrap();
    assert_eq!(
        create_row_label(&name),
        "Create and checkout feature/picker"
    );
}

#[gpui::test]
fn checks_out_the_highlighted_matching_branch_when_enter_is_pressed(cx: &mut TestAppContext) {
    let cwd = "/repo-enter-existing";
    let (picker, cx, setup) = render_picker(cx, cwd, branches(&["main", "feature/picker"]));
    picker.update_in(cx, |picker, window, cx| picker.toggle(window, cx));
    type_query(&picker, "picker", cx);
    let rows = picker.read_with(cx, |picker, cx| picker.rows(cx));
    assert_eq!(rows[0].name, "feature/picker");
    let name = picker
        .read_with(cx, |picker, cx| picker.create_name(cx))
        .unwrap();
    assert_eq!(create_row_label(&name), "Create and checkout picker");

    picker.update_in(cx, |picker, window, cx| picker.enter(window, cx));
    cx.run_until_parked();
    assert_eq!(
        setup.git.calls("git_checkout"),
        vec![vec![
            cwd.to_string(),
            "feature/picker".to_string(),
            "null".to_string(),
            String::new()
        ]]
    );
    assert!(setup.git.calls("git_create_branch").is_empty());
}

#[gpui::test]
fn opens_the_stash_dialog_when_git_refuses_over_local_changes(cx: &mut TestAppContext) {
    let (picker, cx, setup) = render_in_toolbar(cx, "/repo", branches(&["main", "other"]));
    setup.git.fail(
        "git_checkout",
        Some("error: Your local changes to the following files would be overwritten by checkout"),
    );
    picker.update_in(cx, |picker, window, cx| picker.toggle(window, cx));
    type_query(&picker, "other", cx);
    picker.update_in(cx, |picker, window, cx| picker.enter(window, cx));
    cx.run_until_parked();
    let dialog = picker
        .read_with(cx, |picker, _| picker.blocked_dialog().cloned())
        .unwrap();
    assert!(!picker.read_with(cx, |picker, _| picker.is_open()));
    // The dialog covers the window, not just the trigger-sized box the
    // picker renders in.
    let panel = cx.debug_bounds("switch-branch-dialog").unwrap();
    assert_eq!(panel.size.width, px(420.));

    setup.git.fail("git_checkout", None);
    dialog.update(cx, |dialog, cx| dialog.stash(cx));
    cx.run_until_parked();
    assert_eq!(
        setup.git.calls("git_stash"),
        vec![vec![
            "/repo".to_string(),
            "WIP before switching to other".to_string()
        ]]
    );
    assert_eq!(setup.git.calls("git_checkout").len(), 2);
    assert!(picker.read_with(cx, |picker, _| picker.blocked_dialog().is_none()));
}
