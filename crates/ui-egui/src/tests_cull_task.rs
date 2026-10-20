//! Assisted Culling and Group Similar Photos run in the background (they froze the window on a
//! large library): the dialogs start a task with a row in the activity stack, and its result is
//! applied when it finishes.

use std::time::Duration;

use crate::LightcraftApp;
use crate::state::Dialog;

fn demo() -> LightcraftApp {
    LightcraftApp::new(lightcraft_engine::Session::with_demo(), crate::Services { png: None, ..Default::default() })
}

#[test]
fn group_similar_photos_runs_in_the_background_and_stacks_scenes() {
    let mut app = demo();
    let ctx = egui::Context::default();
    app.run("dialog.groupSimilar", serde_json::json!({})).unwrap();
    assert!(matches!(app.ui.dialog, Some(Dialog::GroupSimilar { .. })), "{:?}", app.ui.dialog);
    // everything alike and close enough: the timed photos in view become stacks
    let r = crate::panels::dialogs::confirm_dialog(&mut app, &Dialog::GroupSimilar { gap: 1e9, similarity: 0.0 }).unwrap();
    assert_eq!(r["started"], true, "{r}");
    let rows = app.session.activity.list();
    assert!(rows.iter().any(|r| r.kind == "cull" && r.label == "Group Similar Photos"), "{rows:?}");
    assert_eq!(app.session.catalog.stacks().count(), 0, "nothing changes until it finishes");
    assert!(crate::tasks::wait(&mut app, &ctx, Duration::from_secs(60)));
    assert!(app.session.activity.list().iter().all(|r| r.kind != "cull"), "the row goes with the task");
    let v = app.ui.last_cull.clone().unwrap().unwrap();
    assert!(v["stacks"].as_u64().unwrap() >= 1, "{v}");
    assert_eq!(app.session.catalog.stacks().count() as u64, v["stacks"].as_u64().unwrap());
    assert!(app.ui.toast.as_ref().is_some_and(|(t, _, _)| t.starts_with("Group Similar Photos")), "{:?}", app.ui.toast);
    // one undo step
    app.run("edit.undo", serde_json::json!({})).unwrap();
    assert_eq!(app.session.catalog.stacks().count(), 0);
}

#[test]
fn assisted_culling_runs_in_the_background() {
    let mut app = demo();
    let ctx = egui::Context::default();
    let r = crate::panels::dialogs::confirm_dialog(&mut app, &Dialog::Cull { reject_below: 0.0, pick_best: false }).unwrap();
    assert_eq!(r["started"], true, "{r}");
    assert!(app.session.catalog.photos().all(|p| p.analysis.is_none()), "nothing changes until it finishes");
    // a second run while one is going is refused, not stacked up
    assert!(crate::panels::dialogs::confirm_dialog(&mut app, &Dialog::Cull { reject_below: 0.0, pick_best: false }).is_err());
    assert!(crate::tasks::wait(&mut app, &ctx, Duration::from_secs(60)));
    let n = app.session.visible_cloned().len();
    let scored = app.session.catalog.photos().filter(|p| p.analysis.is_some()).count();
    assert_eq!(scored, n, "every photo in view is scored");
    // measured once: running again reads nothing
    let ids = app.session.visible_cloned();
    assert_eq!(app.session.plan_measure(&ids).to_read(), 0);
}
