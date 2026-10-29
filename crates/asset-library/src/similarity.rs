//! Similarity index over stored embeddings, image search ranking and tag suggestions,
//! as in the original's `app/analysis.py`.
//!
//! Suggestions lean on what the library already knows (folder tags, filenames) rather than
//! on the model knowing TTRPG names: a token that looks like twelve images tagged `illithid`
//! gets `illithid` suggested, whether or not CLIP has heard of illithids.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;
use rusqlite::{Connection, params_from_iter};
use serde::Serialize;

use crate::clip::{EMBED_DIM, from_blob};
use crate::db::{Database, marks};

/// Image searches return a fixed number of best matches; similarity scores fall off
/// smoothly with no natural cut-off.
pub const RANKED_LIMIT: usize = 100;
const K_NEIGHBOURS: usize = 20;
const LOOKALIKE_MIN: f64 = 0.35;

static CAMEL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z])([A-Z])").unwrap());
static WORD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z]+").unwrap());

#[derive(Default)]
pub struct Index {
    ids: Vec<i64>,
    hashes: Vec<String>,
    /// Row-major, EMBED_DIM per asset.
    vectors: Vec<f32>,
    rel_paths: Vec<String>,
    common_words: HashSet<String>,
    pos: HashMap<i64, usize>,
}

impl Index {
    fn vector(&self, n: usize) -> &[f32] {
        &self.vectors[n * EMBED_DIM..(n + 1) * EMBED_DIM]
    }

    fn scores(&self, vec: &[f32]) -> Vec<f32> {
        (0..self.ids.len()).map(|n| dot(self.vector(n), vec)).collect()
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Lazily (re)built after scans and analysis batches.
#[derive(Default)]
pub struct IndexCache {
    index: Mutex<Option<Arc<Index>>>,
}

impl IndexCache {
    pub fn invalidate(&self) {
        *self.index.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn get(&self, db: &Database) -> rusqlite::Result<Arc<Index>> {
        let mut guard = self.index.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(idx) = guard.as_ref() {
            return Ok(Arc::clone(idx));
        }
        let idx = Arc::new(db.tx(|c| build(c))?);
        *guard = Some(Arc::clone(&idx));
        Ok(idx)
    }
}

fn build(c: &Connection) -> rusqlite::Result<Index> {
    let mut stmt = c.prepare(
        "SELECT a.id, a.file_hash, a.rel_path, f.embedding FROM assets a
         LEFT JOIN image_features f ON f.file_hash = a.file_hash
         WHERE a.missing = 0",
    )?;
    let mut idx = Index::default();
    let mut word_counts: HashMap<String, u32> = HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<Vec<u8>>>(3)?))
    })?;
    for row in rows {
        let (id, hash, rel, emb) = row?;
        for w in name_words(&rel).into_iter().collect::<HashSet<_>>() {
            *word_counts.entry(w).or_default() += 1;
        }
        let Some(emb) = emb.filter(|e| e.len() == EMBED_DIM * 4) else { continue };
        idx.pos.insert(id, idx.ids.len());
        idx.ids.push(id);
        idx.hashes.push(hash);
        idx.vectors.extend(from_blob(&emb));
        idx.rel_paths.push(rel);
    }
    idx.common_words = word_counts.into_iter().filter(|(_, n)| *n >= 5).map(|(w, _)| w).collect();
    Ok(idx)
}

/// `[(asset_id, score)]` most similar first.
pub fn rank_by_vector(idx: &Index, vec: &[f32], limit: usize, exclude_hash: Option<&str>) -> Vec<(i64, f32)> {
    let scores = idx.scores(vec);
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
    order
        .into_iter()
        .filter(|&n| exclude_hash != Some(idx.hashes[n].as_str()))
        .take(limit)
        .map(|n| (idx.ids[n], scores[n]))
        .collect()
}

pub fn rank_similar(idx: &Index, asset_id: i64, limit: usize) -> Vec<(i64, f32)> {
    let Some(&n) = idx.pos.get(&asset_id) else { return Vec::new() };
    let mut out = vec![(asset_id, 1.0)];
    out.extend(rank_by_vector(idx, idx.vector(n), limit - 1, Some(&idx.hashes[n])));
    out
}

// ---------------------------------------------------------------- tag suggestions

pub fn name_words(rel_path: &str) -> Vec<String> {
    let stem = Path::new(rel_path).file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let stem = CAMEL_RE.replace_all(stem, "$1 $2");
    WORD_RE.find_iter(&stem).map(|m| m.as_str()).filter(|w| w.len() >= 3).map(str::to_lowercase).collect()
}

/// Filename words plus 2-3 word phrases joined like tags (storm-giant).
pub fn name_grams(rel_path: &str) -> BTreeSet<String> {
    let words = name_words(rel_path);
    let mut grams: BTreeSet<String> = words.iter().cloned().collect();
    for n in [2, 3] {
        grams.extend(words.windows(n).map(|w| w.join("-")));
    }
    grams
}

#[derive(Clone, Debug, Serialize)]
pub struct Suggestion {
    pub tag: String,
    pub score: f64,
    pub source: &'static str,
}

pub fn round_to(x: f64, digits: usize) -> f64 {
    format!("{x:.digits$}").parse().unwrap_or(x)
}

fn query_pairs(c: &Connection, sql: &str, ids: &[i64]) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = c.prepare(&sql.replace("{marks}", &marks(ids.len())))?;
    let rows = stmt.query_map(params_from_iter(ids), |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
    rows
}

fn column(c: &Connection, sql: &str) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = c.prepare(sql)?;
    let rows = stmt.query_map([], |r| r.get(0))?.collect();
    rows
}

/// Per asset: suggestions best first. Sources: filename, look-alike, or both.
pub fn suggest(db: &Database, idx: &Index, asset_ids: &[i64], limit: usize) -> rusqlite::Result<Vec<(i64, Vec<Suggestion>)>> {
    if asset_ids.is_empty() {
        return Ok(Vec::new());
    }
    let (all_tags, ignore, info, rejects) = db.tx(|c| {
        let all_tags = column(c, "SELECT name FROM tags")?;
        let ignore = column(c, "SELECT tag FROM suggestion_ignore")?;
        let info: HashMap<i64, String> =
            query_pairs(c, "SELECT id, rel_path FROM assets WHERE id IN ({marks})", asset_ids)?.into_iter().collect();
        let mut rejects: HashMap<i64, HashSet<String>> = HashMap::new();
        for (id, tag) in query_pairs(c, "SELECT asset_id, tag FROM suggestion_rejects WHERE asset_id IN ({marks})", asset_ids)? {
            rejects.entry(id).or_default().insert(tag);
        }
        Ok::<_, rusqlite::Error>((all_tags, ignore, info, rejects))
    })?;

    // Nearest neighbours for every asset first, so their tags load in one query.
    let mut neighbours: HashMap<i64, Vec<(usize, f32)>> = HashMap::new();
    for &aid in asset_ids {
        let Some(&n) = idx.pos.get(&aid) else { continue };
        let mut scores = idx.scores(idx.vector(n));
        for (j, s) in scores.iter_mut().enumerate() {
            if idx.hashes[j] == idx.hashes[n] {
                *s = -1.0; // itself and exact duplicates
            }
        }
        let k = K_NEIGHBOURS.min(scores.len().saturating_sub(1));
        if k == 0 {
            continue;
        }
        let mut order: Vec<usize> = (0..scores.len()).collect();
        order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
        neighbours.insert(aid, order[..k].iter().map(|&j| (j, scores[j])).collect());
    }

    let mut want: Vec<i64> = asset_ids.to_vec();
    want.extend(neighbours.values().flatten().map(|&(j, _)| idx.ids[j]));
    want.sort_unstable();
    want.dedup();
    let mut tags_of: HashMap<i64, HashSet<String>> = HashMap::new();
    for chunk in want.chunks(900) {
        let pairs = db.tx(|c| {
            query_pairs(
                c,
                "SELECT x.asset_id, t.name FROM asset_tags x JOIN tags t ON t.id = x.tag_id
                 WHERE x.asset_id IN ({marks})",
                chunk,
            )
        })?;
        for (id, tag) in pairs {
            tags_of.entry(id).or_default().insert(tag);
        }
    }

    let empty = HashSet::new();
    let mut out = Vec::new();
    for &aid in asset_ids {
        let Some(rel) = info.get(&aid) else { continue };
        let mut found: HashMap<String, Suggestion> = HashMap::new();
        for g in name_grams(rel) {
            if all_tags.contains(&g) || idx.common_words.contains(&g) {
                let score = if all_tags.contains(&g) { 0.9 } else { 0.6 };
                found.insert(g.clone(), Suggestion { tag: g, score, source: "filename" });
            }
        }

        if let Some(top) = neighbours.get(&aid) {
            let min = top.iter().map(|t| t.1).fold(f32::INFINITY, f32::min);
            // Favour the closest.
            let weights: Vec<f64> = top.iter().map(|&(_, s)| ((s - min).max(0.0) + 1e-3) as f64).collect();
            let mut votes: HashMap<String, f64> = HashMap::new();
            for (&(j, _), wt) in top.iter().zip(&weights) {
                let own = tags_of.get(&idx.ids[j]).unwrap_or(&empty);
                let from_name = name_grams(&idx.rel_paths[j]).into_iter().filter(|g| all_tags.contains(g));
                let labels: HashSet<String> = own.iter().cloned().chain(from_name).collect();
                for lab in labels {
                    // Pack codes like tp9thecamp17 make poor look-alike labels.
                    if !lab.chars().any(|ch| ch.is_ascii_digit()) {
                        *votes.entry(lab).or_default() += wt;
                    }
                }
            }
            let total: f64 = weights.iter().sum();
            for (lab, v) in votes {
                let score = v / total;
                if score < LOOKALIKE_MIN {
                    continue;
                }
                let s = match found.get(&lab) {
                    // Both agree: more confident.
                    Some(f) => Suggestion { tag: lab.clone(), score: (f.score.max(score) + 0.1).min(0.99), source: "filename + look-alike" },
                    None => Suggestion { tag: lab.clone(), score: round_to(score, 2), source: "look-alike" },
                };
                found.insert(lab, s);
            }
        }

        let skip = |t: &str| {
            tags_of.get(&aid).is_some_and(|s| s.contains(t))
                || rejects.get(&aid).is_some_and(|s| s.contains(t))
                || ignore.contains(t)
        };
        let mut ranked: Vec<Suggestion> = found.into_values().filter(|s| !skip(&s.tag)).collect();
        ranked.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.tag.cmp(&b.tag)));
        ranked.truncate(limit);
        ranked.iter_mut().for_each(|s| s.score = round_to(s.score, 2));
        out.push((aid, ranked));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_words_and_grams() {
        assert_eq!(name_words("a/StormGiant_Large_01.png"), ["storm", "giant", "large"]);
        assert_eq!(name_words("x/Dwarf_Fighter_Sword_08.png"), ["dwarf", "fighter", "sword"]);
        let g: Vec<String> = name_grams("x/Storm_Giant_Large.png").into_iter().collect();
        assert_eq!(g, ["giant", "giant-large", "large", "storm", "storm-giant", "storm-giant-large"]);
    }
}
