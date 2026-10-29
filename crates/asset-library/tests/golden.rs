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
