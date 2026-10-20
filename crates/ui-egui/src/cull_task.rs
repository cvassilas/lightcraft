//! Assisted Culling and Group Similar Photos in the background: measuring a large library reads a
//! thumbnail of every photo (a raw decode each), which took minutes on the UI thread and froze the
//! window. The photos are planned here, measured on a worker thread with a row in the activity
//! stack (count and ✕), and the result is applied between frames in one undo step
//! (`lightcraft_engine::culling`).

use lightcraft_catalog::PhotoId;
use serde_json::{Value, json};

use crate::LightcraftApp;

/// What to do with the measured photos.
#[derive(Clone, Debug, PartialEq)]
pub enum Cull {
    /// Assisted Culling (`photo.analyze`).
    Analyze { reject_below: Option<f32>, pick_best: bool },
    /// Group Similar Photos (`stack.similar`).
    Group { gap: f64, similarity: f32 },
}

impl Cull {
    pub fn label(&self) -> &'static str {
        match self {
            Cull::Analyze { .. } => "Assisted Culling",
            Cull::Group { .. } => "Group Similar Photos",
        }
    }
}

/// The photos a culling dialog works on: the selection if several photos, else the view.
pub fn scope(app: &mut LightcraftApp) -> Vec<PhotoId> {
    if app.session.selection.ids.len() > 1 { app.session.selected() } else { app.session.visible_cloned() }
}

/// Start `cull` on `ids` in the background. Its result (`photo.analyze` / `stack.similar`) is kept
/// in `app.ui.last_cull` and told in a toast.
pub fn start(app: &mut LightcraftApp, cull: Cull, ids: Vec<PhotoId>) -> Result<Value, String> {
    let label = cull.label();
    if app.tasks.is_running(label) {
        return Err(format!("{label} is already running"));
    }
    let job = app.session.plan_measure(&ids);
    let (n, to_read) = (job.len(), job.to_read());
    let work = move |task: lightcraft_engine::activity::TaskHandle| {
        task.progress(0, to_read as u64);
        job.run(&|done, total| {
            task.progress(done, total);
            !task.is_cancelled()
        })
    };
    let done = move |app: &mut LightcraftApp, ctx: &egui::Context, m: lightcraft_engine::culling::Measured| {
        app.session.keep_scores(&m);
        let name = crate::i18n::tr(label);
        if m.cancelled {
            app.ui.last_cull = Some(Ok(json!({"cancelled": true})));
            app.toast(ctx, format!("{name} · {}", crate::i18n::tr("Stopped")));
            return;
        }
        let r = match &cull {
            Cull::Analyze { reject_below, pick_best } => app.session.apply_analysis(&m, *reject_below, *pick_best),
            Cull::Group { gap, similarity } => app.session.apply_similar(&m, *gap, *similarity, false),
        }
        .map_err(|e| e.to_string());
        let count = |v: &Value| v.as_u64().unwrap_or(0);
        let text = match (&cull, &r) {
            (_, Err(e)) => format!("{name}: {e}"),
            (Cull::Analyze { .. }, Ok(v)) => format!(
                "{name} · {} · {} {} · {} {} · {} {}",
                crate::i18n::tr_format!("{n} photos", n = v["photos"].as_array().map_or(0, Vec::len)),
                count(&v["groups"]),
                crate::i18n::tr("bursts"),
                count(&v["picked"]),
                crate::i18n::tr("picked"),
                count(&v["rejected"]),
                crate::i18n::tr("rejected")
            ),
            (Cull::Group { .. }, Ok(v)) => {
                format!(
                    "{name} · {} {} · {}",
                    count(&v["stacks"]),
                    crate::i18n::tr("stacks"),
                    crate::i18n::tr_format!("{n} photos", n = count(&v["photos"]))
                )
            }
        };
        app.ui.last_cull = Some(r);
        app.toast(ctx, text);
    };
    crate::tasks::spawn_tracked(app, label, "cull", work, done)?;
    Ok(json!({"started": true, "photos": n, "toRead": to_read}))
}
