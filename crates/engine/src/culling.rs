//! Measuring photos for culling and grouping: focus, clipping and a look-alike signature from a
//! thumbnail-level source (`lightcraft_pipeline::cull`), cached by content in the session.
//!
//! Measuring a library is slow (a raw is decoded for each photo), so it is split in three: a
//! [`Session::plan_measure`] on the session's thread (cheap: cached scores, and where to read the
//! others from), [`MeasureJob::run`] anywhere (a worker thread in the app, with progress and a
//! cancel), and [`Session::keep_scores`] back on the session's thread. The commands
//! (`photo.analyze`, `stack.similar`, `library.findSimilar`) run all three in a row; the desktop app
//! runs the middle one in the background so the window keeps answering.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lightcraft_catalog::{Analysis, Catalog, Flag, Op, PhotoId};
use rayon::prelude::*;
use serde_json::{Value, json};

use crate::media::{SourceLevel, SourceRef, content_key};
use crate::{Result, Session};

/// What culling measures of one photo.
#[derive(Clone, Copy, Debug)]
pub struct Scores {
    /// Focus, 0..100.
    pub sharpness: f32,
    /// Share of clipped pixels, 0..1.
    pub clipped: f32,
    /// Look-alike signature (`lightcraft_pipeline::cull::signature`).
    pub signature: [f32; 64],
}

impl Scores {
    fn of(img: &lightcraft_raster::Rgb32f) -> Scores {
        use lightcraft_pipeline::cull;
        Scores { sharpness: cull::sharpness(img), clipped: cull::clipped(img), signature: cull::signature(img) }
    }
}

/// Photos to measure: those already measured, and where to read the others from.
pub struct MeasureJob {
    known: Vec<(PhotoId, Scores)>,
    todo: Vec<(PhotoId, String, SourceRef)>,
}

/// What a [`MeasureJob`] found.
#[derive(Default)]
pub struct Measured {
    /// In the order the photos were planned.
    pub scores: Vec<(PhotoId, Scores)>,
    /// Newly measured, by content key (for [`Session::keep_scores`]).
    fresh: Vec<(String, Scores)>,
    /// Photos that could not be read, with why.
    pub failed: Vec<(PhotoId, String)>,
    /// Stopped before the end (the photos not reached are in neither list).
    pub cancelled: bool,
}

/// Photos measured at once at most: each holds a decoded raw while it is read.
const WORKERS: usize = 4;

impl MeasureJob {
    /// Photos it has to read (the others are known).
    pub fn to_read(&self) -> usize {
        self.todo.len()
    }

    /// All photos it covers.
    pub fn len(&self) -> usize {
        self.known.len() + self.todo.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read and measure the photos not known yet, a few at a time. `progress(done, total)` is called
    /// after each one (from any thread); returning `false` stops the job.
    pub fn run(self, progress: &(dyn Fn(u64, u64) -> bool + Sync)) -> Measured {
        let total = self.todo.len() as u64;
        let done = AtomicU64::new(0);
        let stop = AtomicBool::new(false);
        let measure = |(id, key, src): &(PhotoId, String, SourceRef)| -> Option<(PhotoId, String, std::result::Result<Scores, String>)> {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            // a photo that panics in a decoder fails alone
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| src.load().map(|img| Scores::of(&img))))
                .unwrap_or_else(|_| Err("the photo could not be read".into()));
            let n = done.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            if !progress(n, total) {
                stop.store(true, Ordering::Relaxed);
            }
            Some((*id, key.clone(), r))
        };
        let results: Vec<Option<(PhotoId, String, std::result::Result<Scores, String>)>> =
            match rayon::ThreadPoolBuilder::new().num_threads(WORKERS).thread_name(|i| format!("lc-measure-{i}")).build() {
                Ok(pool) => pool.install(|| self.todo.par_iter().map(measure).collect()),
                // no threads (the browser build): one after the other
                Err(_) => self.todo.iter().map(measure).collect(),
            };
        let mut out = Measured { scores: self.known, cancelled: stop.load(Ordering::Relaxed), ..Default::default() };
        for (id, key, r) in results.into_iter().flatten() {
            match r {
                Ok(s) => {
                    out.scores.push((id, s));
                    out.fresh.push((key, s));
                }
                Err(e) => out.failed.push((id, e)),
            }
        }
        out
    }
}

impl Session {
    /// Plan measuring `ids` (see the module docs). Photos no longer in the catalog are left out.
    pub fn plan_measure(&mut self, ids: &[PhotoId]) -> MeasureJob {
        let mut job = MeasureJob { known: Vec::new(), todo: Vec::new() };
        for id in ids {
            let Some(p) = self.catalog.photo(*id).cloned() else { continue };
            let key = content_key(&p);
            match self.scores.get(&key) {
                Some(s) => job.known.push((*id, *s)),
                None => job.todo.push((*id, key, self.media.source_ref(&p, SourceLevel::Thumb))),
            }
        }
        job
    }

    /// Keep what a job measured, so measuring the same photos again is instant.
    pub fn keep_scores(&mut self, m: &Measured) {
        for (key, s) in &m.fresh {
            self.scores.insert(key.clone(), *s);
        }
    }

    /// Measure `ids` here and now.
    pub fn measure_now(&mut self, ids: &[PhotoId]) -> Measured {
        let job = self.plan_measure(ids);
        let m = job.run(&|_, _| true);
        self.keep_scores(&m);
        m
    }
}

/// Scenes among measured photos, for Group Similar Photos: runs of photos (in capture order) each
/// taken at most `gap_secs` after the one before and looking at least `min_similarity` like it.
/// Photos without a capture time come last, in file name order, and run by look alone. Deleted
/// photos and photos already in a stack are left out. Each scene of two or more photos is returned
/// sharpest first (the stack's cover), the others in capture order.
pub fn similar_groups(catalog: &Catalog, scores: &[(PhotoId, Scores)], gap_secs: f64, min_similarity: f32) -> Vec<Vec<PhotoId>> {
    let stacked = catalog.stack_index();
    let mut rows: Vec<(Option<i64>, &str, PhotoId, &Scores)> = scores
        .iter()
        .filter_map(|(id, s)| catalog.photo(*id).map(|p| (p, s)))
        .filter(|(p, _)| p.in_library() && !stacked.contains_key(&p.id))
        .map(|(p, s)| (p.captured.as_deref().and_then(lightcraft_catalog::stacks::iso_seconds), p.file_name.as_str(), p.id, s))
        .collect();
    rows.sort_by(|a, b| {
        a.0.unwrap_or(i64::MAX).cmp(&b.0.unwrap_or(i64::MAX)).then_with(|| lightcraft_catalog::query::cmp_name(a.1, b.1)).then(a.2.cmp(&b.2))
    });
    rows.dedup_by_key(|r| r.2);
    let mut groups: Vec<Vec<(PhotoId, &Scores)>> = Vec::new();
    for i in 0..rows.len() {
        let Some(cur) = rows.get(i) else { continue };
        let joins = i.checked_sub(1).and_then(|j| rows.get(j)).is_some_and(|prev| {
            let close = match (prev.0, cur.0) {
                (Some(a), Some(b)) => (b.saturating_sub(a)) as f64 <= gap_secs,
                (None, None) => true,
                _ => false,
            };
            close && lightcraft_pipeline::cull::similarity(&prev.3.signature, &cur.3.signature) >= min_similarity
        });
        match groups.last_mut() {
            Some(g) if joins => g.push((cur.2, cur.3)),
            _ => groups.push(vec![(cur.2, cur.3)]),
        }
    }
    groups
        .into_iter()
        .filter(|g| g.len() >= 2)
        .map(|g| {
            let top =
                g.iter().enumerate().fold(0, |best, (i, (_, s))| if g.get(best).is_some_and(|(_, b)| s.sharpness > b.sharpness) { i } else { best });
            let mut out: Vec<PhotoId> = g.get(top).map(|(id, _)| *id).into_iter().collect();
            out.extend(g.iter().enumerate().filter(|(i, _)| *i != top).map(|(_, (id, _))| *id));
            out
        })
        .collect()
}

/// Group Similar Photos defaults: seconds between consecutive shots of a scene, and how alike.
pub const GROUP_GAP: f64 = 120.0;
pub const GROUP_SIMILARITY: f32 = 0.85;

/// Seconds between shots of the same burst at most (Assisted Culling).
const BURST_GAP: i64 = 10;
/// Signature similarity for "the same scene" in a burst.
const SAME: f32 = 0.93;

fn failed_json(m: &Measured) -> Vec<Value> {
    m.failed.iter().map(|(id, e)| json!([id.0, e])).collect()
}

impl Session {
    /// Assisted Culling with measured photos: keep the scores on them, group look-alike shots taken
    /// within seconds of each other (bursts) and mark the sharpest of each; optionally reject photos
    /// below `reject_below` and pick each burst's best. One undo step.
    pub fn apply_analysis(&mut self, m: &Measured, reject_below: Option<f32>, pick_best: bool) -> Result<Value> {
        let secs = |iso: &str| lightcraft_meta::DateTime::parse_iso(iso).map(|d| d.unix_seconds());
        let mut rows: Vec<(PhotoId, Option<i64>, Scores)> =
            m.scores.iter().filter_map(|(id, sc)| self.catalog.photo(*id).map(|p| (*id, p.captured.as_deref().and_then(secs), *sc))).collect();
        rows.sort_by_key(|r| (r.1.unwrap_or(i64::MAX), r.0));
        // bursts: consecutive (by capture time) look-alike shots
        let mut group_of: Vec<Option<u32>> = vec![None; rows.len()];
        let mut next = 1u32;
        for i in 1..rows.len() {
            let (Some(a), Some(b)) = (rows.get(i - 1), rows.get(i)) else { continue };
            let close = matches!((a.1, b.1), (Some(x), Some(y)) if (y - x).abs() <= BURST_GAP);
            if close && lightcraft_pipeline::cull::similarity(&a.2.signature, &b.2.signature) >= SAME {
                let prev = group_of.get(i - 1).copied().flatten();
                let g = prev.unwrap_or_else(|| {
                    next += 1;
                    next - 1
                });
                if let Some(slot) = group_of.get_mut(i - 1) {
                    *slot = Some(g);
                }
                if let Some(slot) = group_of.get_mut(i) {
                    *slot = Some(g);
                }
            }
        }
        let mut best: HashMap<u32, (usize, f32)> = HashMap::new();
        for (i, g) in group_of.iter().enumerate() {
            if let (Some(g), Some(r)) = (g, rows.get(i)) {
                let e = best.entry(*g).or_insert((i, -1.0));
                if r.2.sharpness > e.1 {
                    *e = (i, r.2.sharpness);
                }
            }
        }
        let mut ops = Vec::new();
        let (mut rejected, mut picked) = (0, 0);
        let mut out = Vec::new();
        for (i, (id, _, sc)) in rows.iter().enumerate() {
            let g = group_of.get(i).copied().flatten();
            let is_best = g.is_some_and(|g| best.get(&g).is_some_and(|b| b.0 == i));
            ops.push(Op::SetAnalysis { id: *id, analysis: Some(Analysis { sharpness: sc.sharpness, clipped: sc.clipped, group: g, best: is_best }) });
            let flag = self.catalog.photo(*id).map(|p| p.flag).unwrap_or_default();
            if reject_below.is_some_and(|t| sc.sharpness < t) && flag != Flag::Reject {
                ops.push(Op::SetFlag { id: *id, flag: Flag::Reject });
                rejected += 1;
            } else if pick_best && is_best && flag == Flag::None {
                ops.push(Op::SetFlag { id: *id, flag: Flag::Pick });
                picked += 1;
            }
            out.push(json!({"id": id.0, "sharpness": (sc.sharpness * 10.0).round() / 10.0, "clipped": sc.clipped, "group": g, "best": is_best}));
        }
        if !ops.is_empty() {
            self.commit("Assisted Culling", Op::Batch { ops })?;
        }
        Ok(json!({"photos": out, "groups": best.len(), "rejected": rejected, "picked": picked, "failed": failed_json(m), "cancelled": m.cancelled}))
    }

    /// Group Similar Photos with measured photos: each scene ([`similar_groups`]) becomes a
    /// collapsed stack with its sharpest photo on top. One undo step. With `preview`, only counts.
    pub fn apply_similar(&mut self, m: &Measured, gap_secs: f64, min_similarity: f32, preview: bool) -> Result<Value> {
        let groups = similar_groups(&self.catalog, &m.scores, gap_secs, min_similarity);
        let photos: usize = groups.iter().map(Vec::len).sum();
        if !preview {
            let mut ops = Vec::new();
            for g in &groups {
                if let Some(top) = g.first() {
                    ops.extend(self.catalog.group_ops(*top, g, true));
                }
            }
            if !ops.is_empty() {
                self.commit("Group Similar Photos", Op::Batch { ops })?;
            }
            crate::cmd::organize::fix_active(self);
        }
        Ok(json!({"stacks": groups.len(), "photos": photos, "failed": failed_json(m), "cancelled": m.cancelled}))
    }
}
