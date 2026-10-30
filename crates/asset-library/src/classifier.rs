//! Per-tag image classifiers, trained on the library's own tags and stored embeddings.
//!
//! One model per asset type: logistic regression for every tag with enough examples, over
//! standardized CLIP embeddings. On the user's tokens, with whole subfolders held out, it beat
//! the look-alike vote (top-1 right 48% vs 42%, shown suggestions right 58% vs 35%; see
//! `tools/eval_tags.py`). Training takes seconds, so models are rebuilt in the background
//! after edits, and only for asset types whose tags changed.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use ndarray::{Array1, Array2, Axis};
use rusqlite::{Connection, params};

use crate::clip::EMBED_DIM;
use crate::db::Database;
use crate::similarity::Index;

/// A tag needs this many examples, in at least `MIN_FOLDERS` folders (else it's just one
/// folder's name, which filename suggestions already cover).
const MIN_POSITIVES: usize = 15;
const MIN_FOLDERS: usize = 2;
/// Bounds training time on huge types (92k pack props): most common tags, sampled assets.
const MAX_LABELS: usize = 150;
const MAX_SAMPLES: usize = 12_000;
const STEPS: usize = 100;
const LEARNING_RATE: f32 = 0.5;
const L2: f32 = 1e-2;
/// Suggest a tag when its probability reaches this.
pub const THRESHOLD: f32 = 0.5;

pub struct Model {
    pub labels: Vec<String>,
    mean: Array1<f32>,
    scale: Array1<f32>,
    /// (EMBED_DIM + 1) x labels; the last row is the bias.
    weights: Array2<f32>,
}

impl Model {
    /// Probability of each label for one embedding.
    pub fn predict(&self, vec: &[f32]) -> Vec<f32> {
        let x: Array1<f32> = (&Array1::from_iter(vec.iter().copied()) - &self.mean) / &self.scale;
        let mut z = x.dot(&self.weights.slice(ndarray::s![..EMBED_DIM, ..]));
        z += &self.weights.row(EMBED_DIM);
        z.iter().map(|&v| sigmoid(v)).collect()
    }
}

fn sigmoid(z: f32) -> f32 {
    1.0 / (1.0 + (-z.clamp(-30.0, 30.0)).exp())
}

/// What a type's model was trained on: retrain only when this changes.
type Signature = (i64, i64, f64);

#[derive(Default)]
pub struct Models {
    by_type: HashMap<String, (Signature, Arc<Model>)>,
}

impl Models {
    pub fn get(&self, asset_type: &str) -> Option<&Model> {
        self.by_type.get(asset_type).map(|(_, m)| m.as_ref())
    }
}

#[derive(Default)]
pub struct Classifiers {
    models: Mutex<Arc<Models>>,
}

impl Classifiers {
    pub fn current(&self) -> Arc<Models> {
        Arc::clone(&self.models.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Retrain the types whose tags or embeddings changed since the last run. Data is read in
    /// short transactions; training runs outside them, so the app stays responsive.
    pub fn retrain(&self, db: &Database, idx: &Index) -> rusqlite::Result<()> {
        let old = self.current();
        let mut next = Models::default();
        let types: Vec<String> = db.tx(|c| {
            c.prepare("SELECT DISTINCT asset_type FROM assets WHERE missing = 0")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()
        })?;
        for t in types {
            let sig = db.tx(|c| signature(c, &t, idx))?;
            if let Some((s, m)) = old.by_type.get(&t).filter(|(s, _)| *s == sig) {
                next.by_type.insert(t, (*s, Arc::clone(m)));
            } else if let Some(set) = db.tx(|c| load(c, &t, idx))? {
                next.by_type.insert(t, (sig, Arc::new(fit(set))));
            }
        }
        *self.models.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
        Ok(())
    }
}

fn signature(c: &Connection, asset_type: &str, idx: &Index) -> rusqlite::Result<Signature> {
    let (n, sum): (i64, f64) = c.query_row(
        "SELECT count(*), total(x.asset_id * 1000003 + x.tag_id) FROM asset_tags x
         JOIN assets a ON a.id = x.asset_id AND a.asset_type = ? AND a.missing = 0",
        params![asset_type],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((idx.len() as i64, n, sum))
}

/// Standardized embeddings (with a bias column), 0/1 targets and their labels.
pub struct TrainingSet {
    x: Array2<f32>,
    y: Array2<f32>,
    labels: Vec<String>,
    mean: Array1<f32>,
    scale: Array1<f32>,
}

/// `asset_type`'s training data, or `None` if no tag has enough examples. Only assets with at
/// least one of the learned tags are trained on: an untagged asset (a pack nobody has tagged
/// yet) isn't evidence against any tag, and counting it as one hides exactly the assets that
/// need suggestions.
pub fn load(c: &Connection, asset_type: &str, idx: &Index) -> rusqlite::Result<Option<TrainingSet>> {
    let mut stmt = c.prepare("SELECT id, rel_path FROM assets WHERE asset_type = ? AND missing = 0 ORDER BY id")?;
    let all: Vec<(i64, String)> = stmt
        .query_map(params![asset_type], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter(|r| r.as_ref().map_or(true, |(id, _)| idx.vector_of(*id).is_some()))
        .collect::<rusqlite::Result<_>>()?;
    let row_of: HashMap<i64, usize> = all.iter().enumerate().map(|(i, (id, _))| (*id, i)).collect();

    let mut tagged: HashMap<String, Vec<usize>> = HashMap::new();
    let mut stmt = c.prepare(
        "SELECT x.asset_id, t.name FROM asset_tags x JOIN tags t ON t.id = x.tag_id
         JOIN assets a ON a.id = x.asset_id AND a.asset_type = ? AND a.missing = 0",
    )?;
    for row in stmt.query_map(params![asset_type], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (id, tag) = row?;
        if let Some(&i) = row_of.get(&id) {
            tagged.entry(tag).or_default().push(i);
        }
    }
    let folder = |i: usize| all[i].1.rsplit_once('/').map_or("", |(d, _)| d);
    let mut labels: Vec<(String, Vec<usize>)> = tagged
        .into_iter()
        .filter(|(t, rows)| {
            // Pack codes like tp9thecamp17 make poor labels (as for look-alike votes).
            !t.chars().any(|ch| ch.is_ascii_digit())
                && rows.len() >= MIN_POSITIVES
                && rows.iter().map(|&i| folder(i)).collect::<HashSet<_>>().len() >= MIN_FOLDERS
        })
        .collect();
    labels.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    labels.truncate(MAX_LABELS);

    // Training rows: assets with a learned tag, evenly sampled down to MAX_SAMPLES.
    let mut rows: Vec<usize> = labels.iter().flat_map(|(_, r)| r.iter().copied()).collect();
    rows.sort_unstable();
    rows.dedup();
    if rows.len() > MAX_SAMPLES {
        let step = rows.len() as f64 / MAX_SAMPLES as f64;
        rows = (0..MAX_SAMPLES).map(|i| rows[(i as f64 * step) as usize]).collect();
    }
    // A tag on every training row can't be told apart from anything.
    labels.retain(|(_, r)| r.len() < rows.len());
    if labels.is_empty() {
        return Ok(None);
    }
    let pos_of: HashMap<usize, usize> = rows.iter().enumerate().map(|(p, &i)| (i, p)).collect();
    let assets: Vec<&(i64, String)> = rows.iter().map(|&i| &all[i]).collect();
    let labels: Vec<(String, Vec<usize>)> =
        labels.into_iter().map(|(t, r)| (t, r.iter().filter_map(|i| pos_of.get(i).copied()).collect())).collect();

    let n = assets.len();
    let mut x = Array2::<f32>::zeros((n, EMBED_DIM + 1));
    for (i, (id, _)) in assets.iter().enumerate() {
        let v = idx.vector_of(*id).unwrap_or(&[]);
        x.row_mut(i).slice_mut(ndarray::s![..EMBED_DIM]).assign(&ndarray::ArrayView1::from(v));
    }
    let feats = x.slice(ndarray::s![.., ..EMBED_DIM]);
    let mean = feats.mean_axis(Axis(0)).unwrap_or_else(|| Array1::zeros(EMBED_DIM));
    let scale = feats.std_axis(Axis(0), 0.0) + 1e-6;
    {
        let mut f = x.slice_mut(ndarray::s![.., ..EMBED_DIM]);
        f -= &mean;
        f /= &scale;
    }
    x.column_mut(EMBED_DIM).fill(1.0);

    let mut y = Array2::<f32>::zeros((n, labels.len()));
    for (j, (_, rows)) in labels.iter().enumerate() {
        for &i in rows {
            y[[i, j]] = 1.0;
        }
    }

    Ok(Some(TrainingSet { x, y, labels: labels.into_iter().map(|(t, _)| t).collect(), mean, scale }))
}

/// One-vs-rest logistic regression, every label at once, full-batch gradient descent.
pub fn fit(set: TrainingSet) -> Model {
    let TrainingSet { x, y, labels, mean, scale } = set;
    let n = x.nrows();
    let mut w = Array2::<f32>::zeros((EMBED_DIM + 1, labels.len()));
    let xt = x.t();
    for _ in 0..STEPS {
        let mut g = x.dot(&w);
        g.mapv_inplace(sigmoid);
        g -= &y;
        let mut grad = xt.dot(&g);
        grad /= n as f32;
        grad.scaled_add(L2, &w);
        w.scaled_add(-LEARNING_RATE, &grad);
    }
    Model { labels, mean, scale, weights: w }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::to_blob;
    use crate::db::get_or_create_tags;
    use crate::similarity::IndexCache;

    /// Unit vector near axis `axis`, jittered by `seed`.
    fn embedding(axis: usize, seed: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..EMBED_DIM).map(|i| (((i * 7919 + seed * 104_729) % 1000) as f32 / 1000.0 - 0.5) * 0.4).collect();
        v[axis] += 3.0;
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / norm).collect()
    }

    #[test]
    fn learns_tags_from_embeddings() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("library.db")).unwrap();
        db.tx(|c| {
            c.execute("INSERT INTO sources (path, name) VALUES ('/x', 'x')", [])?;
            // 40 dragons and 40 goblins, each spread over 4 folders; "lair" is one folder only.
            for i in 0..80 {
                let (axis, tag, hash) = if i < 40 { (0, "dragon", format!("d{i}")) } else { (1, "goblin", format!("g{i}")) };
                c.execute(
                    "INSERT INTO assets (source_id, rel_path, file_hash, size, mtime, name, asset_type)
                     VALUES (1, ?, ?, 1, 0, ?, 'token')",
                    params![format!("f{}/{hash}.png", i % 4), hash, hash],
                )?;
                let id = c.last_insert_rowid();
                c.execute("INSERT INTO image_features (file_hash, embedding) VALUES (?, ?)", params![hash, to_blob(&embedding(axis, i))])?;
                let mut tags = vec![tag.to_string()];
                if i % 4 == 0 && i < 40 {
                    tags.push("lair".into());
                }
                for t in get_or_create_tags(c, &tags)? {
                    c.execute("INSERT INTO asset_tags VALUES (?, ?)", params![id, t])?;
                }
            }
            // A new, untagged pack of dragons: not evidence against "dragon".
            for i in 0..30 {
                let hash = format!("new{i}");
                c.execute(
                    "INSERT INTO assets (source_id, rel_path, file_hash, size, mtime, name, asset_type)
                     VALUES (1, ?, ?, 1, 0, ?, 'token')",
                    params![format!("new/{hash}.png"), hash, hash],
                )?;
                c.execute("INSERT INTO image_features (file_hash, embedding) VALUES (?, ?)", params![hash, to_blob(&embedding(0, 500 + i))])?;
            }
            Ok::<_, rusqlite::Error>(())
        })
        .unwrap();

        let idx = IndexCache::default().get(&db).unwrap();
        let classifiers = Classifiers::default();
        classifiers.retrain(&db, &idx).unwrap();
        let models = classifiers.current();
        let m = models.get("token").expect("a token model");
        assert_eq!(m.labels, ["dragon", "goblin"]); // "lair" is one folder's name
        let p = m.predict(&embedding(0, 999));
        assert!(p[0] > 0.9 && p[1] < 0.1, "{p:?}");
        let p = m.predict(&embedding(1, 998));
        assert!(p[1] > 0.9 && p[0] < 0.1, "{p:?}");

        // Nothing changed: the same model is kept rather than retrained.
        classifiers.retrain(&db, &idx).unwrap();
        assert!(std::ptr::eq(classifiers.current().get("token").unwrap(), m));
    }

    /// Times training on a copy of a real library:
    /// `LIBRARY_DB=<copy of library.db> cargo test --release -p asset-library real_library -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_library() {
        let db = Database::open(std::path::Path::new(&std::env::var("LIBRARY_DB").expect("set LIBRARY_DB"))).unwrap();
        let idx = IndexCache::default().get(&db).unwrap();
        for t in ["token", "prop", "texture", "other", "tile", "map"] {
            let start = std::time::Instant::now();
            let Some(set) = db.tx(|c| load(c, t, &idx)).unwrap() else { continue };
            let (n, labels) = (set.x.nrows(), set.labels.len());
            let loaded = start.elapsed();
            fit(set);
            println!("{t:<8} {n:>6} images {labels:>4} tags: load {:.1} s, train {:.1} s", loaded.as_secs_f64(), (start.elapsed() - loaded).as_secs_f64());
        }
    }
}
