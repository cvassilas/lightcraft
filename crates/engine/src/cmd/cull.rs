//! Assisted culling: score photos for focus and clipping, group similar shots taken close
//! together (bursts) and mark the sharpest of each group; optionally reject blurry ones and pick
//! each group's best. Scores are kept on the photos (rules / smart albums: `sharpness`,
//! `bestOfGroup`).

use lightcraft_catalog::PhotoId;
use serde_json::{Value, json};

use super::{CommandSpec, always, cmd};
use crate::culling::{GROUP_GAP, GROUP_SIMILARITY};
use crate::{Result, Session};

/// The photos a culling command works on: `ids`, else the selection if several photos, else the view.
fn pool(s: &mut Session, p: &Value) -> Result<Vec<PhotoId>> {
    if p.get("ids").is_some() || s.selection.ids.len() > 1 { s.targets(p) } else { Ok(s.visible_cloned()) }
}

fn analyze(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = pool(s, p)?;
    let reject_below = p.get("rejectBelow").and_then(Value::as_f64).map(|v| v as f32);
    let pick_best = p.get("pickBest").and_then(Value::as_bool).unwrap_or(false);
    let m = s.measure_now(&ids);
    s.apply_analysis(&m, reject_below, pick_best)
}

/// Group Similar Photos: scenes (look-alike shots taken close together) become stacks.
fn group_similar(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = pool(s, p)?;
    let gap = p.get("gap").and_then(Value::as_f64).unwrap_or(GROUP_GAP).max(0.0);
    let min = p.get("similarity").and_then(Value::as_f64).unwrap_or(GROUP_SIMILARITY as f64).clamp(0.0, 1.0) as f32;
    let preview = p.get("preview").and_then(Value::as_bool).unwrap_or(false);
    let m = s.measure_now(&ids);
    s.apply_similar(&m, gap, min, preview)
}

/// Find Similar: photos that look like `id` (signatures from thumbnail-level sources, cached by
/// content), most similar first; the view is filtered to them.
fn find_similar(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.findSimilar";
    let id = p.get("id").and_then(Value::as_u64).map(PhotoId).or(s.active()).ok_or_else(|| super::bad(C, "no photo"))?;
    let min = p.get("similarity").and_then(Value::as_f64).unwrap_or(0.8) as f32;
    let all: Vec<PhotoId> = s.catalog.photos().filter(|q| q.in_library()).map(|q| q.id).collect();
    let me = s.measure_now(&[id]).scores.first().map(|(_, sc)| sc.signature).ok_or_else(|| super::bad(C, "can't read the photo"))?;
    let m = s.measure_now(&all);
    let mut hits: Vec<(PhotoId, f32)> =
        m.scores.iter().map(|(q, sc)| (*q, lightcraft_pipeline::cull::similarity(&me, &sc.signature))).filter(|(_, sim)| *sim >= min).collect();
    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    if p.get("filter").and_then(Value::as_bool).unwrap_or(true) {
        s.filter.only = hits.iter().map(|h| h.0).collect();
    }
    Ok(json!({"photos": hits.iter().map(|(i, v)| json!({"id": i.0, "similarity": (v * 1000.0).round() / 1000.0})).collect::<Vec<_>>()}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "library.findSimilar",
            "Find Similar Photos",
            ["Photo"],
            None,
            "{id?, similarity?: 0..1 (0.8), filter?: bool (true)} — photos that look like the active one (composition and tones), most similar first; filters the view to them (Clear Filters to go back) → {photos: [{id, similarity}]}",
            always,
            find_similar
        ),
        cmd!(
            "photo.analyze",
            "Assisted Culling",
            [],
            None,
            "{ids?, rejectBelow?: sharpness 0..100, pickBest?: bool} — score the selected photos (else all in view) for focus (0..100) and clipping, group look-alike shots taken within 10 s and mark the sharpest of each; optionally reject blurry photos and pick each group's best; one undo step → {photos: [{id, sharpness, clipped, group, best}], groups, rejected, picked}",
            always,
            analyze
        ),
        cmd!(
            "stack.similar",
            "Group Similar Photos",
            [],
            None,
            "{ids?, gap?: seconds between consecutive shots (120), similarity?: 0..1 (0.85), preview?: bool} — sort the selected photos (else all in view) into scenes: look-alike shots each taken within `gap` of the one before; each scene becomes a collapsed stack with its sharpest photo on top; photos already stacked are left alone; one undo step → {stacks, photos, failed, cancelled}",
            always,
            group_similar
        ),
    ]
}
