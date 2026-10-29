//! Compare against fixtures from the real library (tools/make_golden.py). The fixtures are
//! gitignored, so each test skips itself when they're absent.

use std::path::PathBuf;

use serde_json::Value;

fn golden() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden");
    if dir.join("features.json").exists() {
        Some(dir)
    } else {
        eprintln!("skipped: {} not present", dir.display());
        None
    }
}

fn features(dir: &PathBuf) -> Vec<Value> {
    serde_json::from_str(&std::fs::read_to_string(dir.join("features.json")).unwrap()).unwrap()
}

/// Colours from the same thumbnails Python analysed. Differences come only from resampling
/// and WebP decoding, so the dominant colour must almost always agree.
#[test]
fn colours_match_python() {
    let Some(dir) = golden() else { return };
    let feats = features(&dir);
    let (mut l1_sum, mut l1_max, mut dominant_same) = (0.0, 0.0f64, 0);
    for f in &feats {
        let hash = f["hash"].as_str().unwrap();
        let img = image::open(dir.join("thumbs").join(format!("{hash}.webp"))).unwrap();
        let ours = asset_library::analysis::color_profile(&img);
        let theirs = f["colors"].as_object().unwrap();
        let get = |k: &str| ours.iter().find(|(n, _)| *n == k).map_or(0.0, |(_, v)| *v);
        let l1: f64 = asset_library::analysis::COLORS
            .iter()
            .map(|c| (get(c) - theirs.get(*c).and_then(Value::as_f64).unwrap_or(0.0)).abs())
            .sum();
        l1_sum += l1;
        l1_max = l1_max.max(l1);
        let top = |it: &mut dyn Iterator<Item = (&str, f64)>| it.max_by(|a, b| a.1.total_cmp(&b.1)).map(|t| t.0.to_string());
        if top(&mut ours.iter().map(|(k, v)| (*k, *v))) == top(&mut theirs.iter().map(|(k, v)| (k.as_str(), v.as_f64().unwrap()))) {
            dominant_same += 1;
        }
    }
    let n = feats.len() as f64;
    eprintln!("colours: mean L1 {:.4}, max L1 {:.4}, dominant agrees {dominant_same}/{}", l1_sum / n, l1_max, feats.len());
    assert!(l1_sum / n < 0.02, "mean L1 distance too large");
    assert!(dominant_same as f64 / n >= 0.97, "dominant colour disagrees too often");
}

fn models() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
    dir.join("models--Qdrant--clip-ViT-B-32-vision").exists().then_some(dir).or_else(|| {
        eprintln!("skipped: CLIP models not in models/");
        None
    })
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A copy of the golden library, opened through the Rust schema code.
fn golden_db(dir: &PathBuf) -> (tempfile::TempDir, asset_library::db::Database) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::copy(dir.join("library.db"), tmp.path().join("library.db")).unwrap();
    let db = asset_library::db::Database::open(&tmp.path().join("library.db")).unwrap();
    (tmp, db)
}

/// Image embeddings of the same thumbnails Python embedded.
#[test]
fn image_embeddings_match_python() {
    let (Some(dir), Some(models)) = (golden(), models()) else { return };
    let feats = features(&dir);
    let raw = std::fs::read(dir.join("embeddings.f32")).unwrap();
    let theirs = asset_library::clip::from_blob(&raw);
    let clip = asset_library::clip::Clip::new(models);
    let mut sims = Vec::new();
    for (chunk_no, chunk) in feats.chunks(32).enumerate() {
        let imgs = chunk
            .iter()
            .map(|f| image::open(dir.join("thumbs").join(format!("{}.webp", f["hash"].as_str().unwrap()))).unwrap())
            .collect();
        for (i, v) in clip.embed_images(imgs, 4).unwrap().iter().enumerate() {
            let n = chunk_no * 32 + i;
            sims.push(cosine(v, &theirs[n * 512..(n + 1) * 512]));
        }
    }
    sims.sort_by(f32::total_cmp);
    let mean = sims.iter().sum::<f32>() / sims.len() as f32;
    eprintln!("image embeddings: mean cosine {mean:.4}, min {:.4}", sims[0]);
    assert!(mean > 0.99 && sims[0] > 0.95);
}

/// Text embeddings, and their top-20 over the whole library (using Python's stored vectors).
#[test]
fn text_search_matches_python() {
    let (Some(dir), Some(models)) = (golden(), models()) else { return };
    let text: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(dir.join("text.json")).unwrap()).unwrap();
    let (_tmp, db) = golden_db(&dir);
    let cache = asset_library::similarity::IndexCache::default();
    let idx = cache.get(&db).unwrap();
    let clip = asset_library::clip::Clip::new(models);
    let mut overlaps = Vec::new();
    for q in &text {
        let theirs: Vec<f32> = q["embedding"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap() as f32).collect();
        let ours = clip.embed_text(q["query"].as_str().unwrap()).unwrap();
        let top: Vec<i64> = asset_library::similarity::rank_by_vector(&idx, &ours, 20, None).iter().map(|r| r.0).collect();
        let golden_top: Vec<i64> = q["top"].as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect();
        let overlap = top.iter().filter(|id| golden_top.contains(id)).count();
        eprintln!("text {:?}: cosine {:.4}, top-20 overlap {overlap}/20", q["query"].as_str().unwrap(), cosine(&ours, &theirs));
        assert!(cosine(&ours, &theirs) > 0.99);
        overlaps.push(overlap);
    }
    assert!(overlaps.iter().sum::<usize>() as f64 / overlaps.len() as f64 >= 18.0);
}

/// Suggestions on the same database; only ties may differ.
#[test]
fn suggestions_match_python() {
    let Some(dir) = golden() else { return };
    let golden_sugg: serde_json::Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(dir.join("suggestions.json")).unwrap()).unwrap();
    let (_tmp, db) = golden_db(&dir);
    let cache = asset_library::similarity::IndexCache::default();
    let idx = cache.get(&db).unwrap();
    let ids: Vec<i64> = golden_sugg.keys().map(|k| k.parse().unwrap()).collect();
    let ours = asset_library::similarity::suggest(&db, &idx, &ids, 12).unwrap();
    let (mut same_sets, mut score_diff_max) = (0, 0.0f64);
    for (aid, list) in &ours {
        let theirs = golden_sugg[&aid.to_string()].as_array().unwrap();
        let their_tags: std::collections::BTreeSet<&str> = theirs.iter().map(|s| s["tag"].as_str().unwrap()).collect();
        let our_tags: std::collections::BTreeSet<&str> = list.iter().map(|s| s.tag.as_str()).collect();
        if our_tags == their_tags {
            same_sets += 1;
        }
        for s in list {
            if let Some(t) = theirs.iter().find(|t| t["tag"] == s.tag.as_str()) {
                score_diff_max = score_diff_max.max((t["score"].as_f64().unwrap() - s.score).abs());
                assert_eq!(t["source"], s.source, "source differs for {aid} {}", s.tag);
            }
        }
    }
    eprintln!("suggestions: identical tag sets {same_sets}/{}, max score diff {score_diff_max:.3}", ours.len());
    assert!(same_sets as f64 / ours.len() as f64 >= 0.95);
    assert!(score_diff_max <= 0.011);
}
