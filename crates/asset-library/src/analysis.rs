//! Image analysis: dominant colours (always) and CLIP embeddings (when AI is on), from the
//! cached thumbnails. Ported from the original's `app/analysis.py`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::DynamicImage;
use rusqlite::params;
use serde::Serialize;

use crate::clip::{Clip, IMAGE_MODEL, to_blob};
use crate::db::Database;
use crate::similarity::IndexCache;
use crate::thumbs::{Thumbs, default_threads};

#[derive(Clone, Default, Serialize)]
pub struct AnalysisStatus {
    pub running: bool,
    pub total: u64,
    pub done: u64,
    pub ai_available: bool,
    pub error: String,
    /// What's happening besides counting, e.g. a model download. Not in the original.
    pub message: String,
}

pub struct Analysis {
    db: Arc<Database>,
    thumbs: Arc<Thumbs>,
    pub clip: Clip,
    pub index: IndexCache,
    status: Mutex<AnalysisStatus>,
    ai: AtomicBool,
    pub batch: AtomicUsize,
    pub threads: AtomicUsize,
}

impl Analysis {
    pub fn new(db: Arc<Database>, thumbs: Arc<Thumbs>, clip: Clip, ai: bool) -> Self {
        Self {
            db,
            thumbs,
            clip,
            index: IndexCache::default(),
            status: Mutex::new(AnalysisStatus { ai_available: ai, ..Default::default() }),
            ai: AtomicBool::new(ai),
            batch: AtomicUsize::new(64),
            threads: AtomicUsize::new(default_threads()),
        }
    }

    pub fn ai_available(&self) -> bool {
        self.ai.load(Ordering::Relaxed)
    }

    pub fn set_ai(&self, on: bool) {
        self.ai.store(on, Ordering::Relaxed);
        self.status().ai_available = on;
    }

    pub fn status(&self) -> MutexGuard<'_, AnalysisStatus> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Compute colours and (if AI is on) embeddings for every hash lacking them. Runs on the
    /// job thread after thumbnails; the caller owns the `running` flag.
    pub fn analyze_pending(&self, wait: &dyn Fn()) {
        let ai = self.ai_available();
        let rows: Vec<String> = self
            .db
            .tx(|c| {
                let mut stmt = c.prepare(
                    "SELECT DISTINCT a.file_hash
                     FROM assets a LEFT JOIN image_features f ON f.file_hash = a.file_hash
                     WHERE a.missing = 0
                       AND (f.file_hash IS NULL OR f.colors IS NULL
                            OR (? AND (f.embedding IS NULL OR f.embed_model IS NOT ?)))",
                )?;
                let rows = stmt.query_map(params![ai, IMAGE_MODEL], |r| r.get(0))?.collect();
                rows
            })
            .unwrap_or_default();
        let todo: Vec<String> = rows.into_iter().filter(|h| self.thumbs.path(h).exists()).collect();
        {
            let mut st = self.status();
            *st = AnalysisStatus { running: st.running, total: todo.len() as u64, ai_available: ai, ..Default::default() };
        }
        if todo.is_empty() {
            return;
        }
        if let Err(e) = self.analyze(&todo, ai, wait) {
            // e.g. a database error; colours saved so far are kept
            self.status().error = e.to_string();
        }
        self.clip.release_image_model();
        self.index.invalidate();
    }

    fn analyze(&self, hashes: &[String], mut use_ai: bool, wait: &dyn Fn()) -> anyhow::Result<()> {
        let batch = self.batch.load(Ordering::Relaxed).max(1);
        for (n, chunk) in hashes.chunks(batch).enumerate() {
            wait();
            let images: Vec<Option<DynamicImage>> =
                chunk.iter().map(|h| image::open(self.thumbs.path(h)).ok()).collect();
            let colors: Vec<Option<String>> =
                images.iter().map(|im| im.as_ref().map(|im| colors_json(&color_profile(im)))).collect();
            let mut vectors: Vec<Option<Vec<u8>>> = vec![None; chunk.len()];
            if use_ai {
                if !self.clip.image_model_loaded() && !self.clip.image_model_cached() {
                    self.status().message = "Downloading the AI model (about 600 MB, once)".into();
                }
                let present: Vec<usize> = (0..chunk.len()).filter(|&i| images[i].is_some()).collect();
                let batch_imgs = present.iter().map(|&i| images[i].clone().expect("present")).collect();
                let result = self.clip.embed_images(batch_imgs, self.threads.load(Ordering::Relaxed));
                self.status().message.clear();
                match result {
                    Ok(vecs) => {
                        for (&i, v) in present.iter().zip(vecs) {
                            vectors[i] = Some(to_blob(&v));
                        }
                    }
                    Err(e) => {
                        self.status().error = format!("Image model unavailable ({e}); colours only");
                        use_ai = false;
                    }
                }
            }
            self.db.tx(|c| {
                let mut stmt = c.prepare(
                    "INSERT INTO image_features (file_hash, colors, embedding, embed_model)
                     VALUES (?, ?, ?, ?)
                     ON CONFLICT(file_hash) DO UPDATE SET colors = excluded.colors,
                       embedding = coalesce(excluded.embedding, embedding),
                       embed_model = coalesce(excluded.embed_model, embed_model)",
                )?;
                for ((h, col), v) in chunk.iter().zip(&colors).zip(&vectors) {
                    if let Some(col) = col {
                        let model = v.as_ref().map(|_| IMAGE_MODEL);
                        stmt.execute(params![h, col, v, model])?;
                    }
                }
                Ok::<_, rusqlite::Error>(())
            })?;
            self.status().done += chunk.len() as u64;
            if n % 10 == 9 {
                self.index.invalidate(); // let search use what's done so far
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- colours

pub const COLORS: &[&str] = &[
    "red", "orange", "yellow", "green", "teal", "blue", "purple", "pink", "brown", "black", "white", "gray",
];

/// Fraction of opaque pixels falling in each named colour, in order of first appearance.
pub fn color_profile(image: &DynamicImage) -> Vec<(&'static str, f64)> {
    // Pillow: convert("RGBA").resize((64, 64)) -- bicubic on premultiplied alpha.
    let src: DynamicImage = image.to_rgba8().into();
    let mut im = DynamicImage::new_rgba8(64, 64);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom));
    if Resizer::new().resize(&src, &mut im, &opts).is_err() {
        return Vec::new();
    }
    let mut counts: Vec<(&'static str, u32)> = Vec::new();
    let mut total = 0u32;
    for p in im.as_rgba8().into_iter().flat_map(|b| b.pixels()) {
        if p[3] <= 127 {
            continue;
        }
        let name = color_name(p[0], p[1], p[2]);
        total += 1;
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, c)) => *c += 1,
            None => counts.push((name, 1)),
        }
    }
    counts
        .into_iter()
        .map(|(name, n)| (name, n as f64 / total as f64))
        .filter(|(_, f)| *f >= 0.02)
        // Python's round(x, 3): correctly rounded from the exact binary value.
        .map(|(name, f)| (name, format!("{f:.3}").parse().unwrap_or(f)))
        .collect()
}

/// Python's `json.dumps` of the profile, so stored values look like the original's.
pub fn colors_json(profile: &[(&str, f64)]) -> String {
    let items: Vec<String> =
        profile.iter().map(|(k, v)| format!("\"{k}\": {}", serde_json::Value::from(*v))).collect();
    format!("{{{}}}", items.join(", "))
}

/// Pillow's 8-bit RGB -> HSV conversion (`rgb2hsv_row`).
fn pil_hsv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let maxc = r.max(g).max(b);
    let minc = r.min(g).min(b);
    if maxc == minc {
        return (0, 0, maxc);
    }
    let cr = (maxc - minc) as f32;
    let s = cr / maxc as f32;
    let rc = (maxc - r) as f32 / cr;
    let gc = (maxc - g) as f32 / cr;
    let bc = (maxc - b) as f32 / cr;
    // C promotes `2.0 + rc - bc` to double before storing it in a float; match that.
    let h = if r == maxc {
        bc - gc
    } else if g == maxc {
        (2.0 + rc as f64 - bc as f64) as f32
    } else {
        (4.0 + gc as f64 - rc as f64) as f32
    };
    let h = ((h as f64 / 6.0 + 1.0) % 1.0) as f32;
    let clip8 = |x: f32| ((x as f64 * 255.0) as i32).clamp(0, 255) as u8;
    (clip8(h), clip8(s), maxc)
}

fn color_name(r: u8, g: u8, b: u8) -> &'static str {
    let (h8, s8, v8) = pil_hsv(r, g, b);
    let (h, s, v) = (h8 as f32 / 255.0 * 360.0, s8 as f32 / 255.0, v8 as f32 / 255.0);
    if v < 0.18 {
        return "black";
    }
    if s < 0.18 {
        return if v > 0.82 { "white" } else { "gray" };
    }
    // Shades that hue alone gets wrong.
    if (15.0..70.0).contains(&h) && (v < 0.5 || (s < 0.5 && v < 0.7)) {
        return "brown";
    }
    if (h < 15.0 || h >= 345.0) && s < 0.55 && v > 0.7 {
        return "pink";
    }
    if (290.0..345.0).contains(&h) && v < 0.4 {
        return "purple";
    }
    const BANDS: &[(f32, f32, &str)] = &[
        (0.0, 15.0, "red"),
        (15.0, 40.0, "orange"),
        (40.0, 70.0, "yellow"),
        (70.0, 165.0, "green"),
        (165.0, 200.0, "teal"),
        (200.0, 255.0, "blue"),
        (255.0, 290.0, "purple"),
        (290.0, 345.0, "pink"),
        (345.0, 361.0, "red"),
    ];
    BANDS.iter().find(|(lo, hi, _)| h >= *lo && h < *hi).map_or("", |b| b.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(rgba: [u8; 4]) -> DynamicImage {
        image::RgbaImage::from_pixel(8, 8, image::Rgba(rgba)).into()
    }

    #[test]
    fn profiles_solid_colours() {
        assert_eq!(color_profile(&solid([40, 80, 220, 255])), [("blue", 1.0)]);
        assert_eq!(color_profile(&solid([120, 70, 30, 255])), [("brown", 1.0)]);
        assert_eq!(color_profile(&solid([230, 110, 190, 255])), [("pink", 1.0)]);
        assert!(color_profile(&solid([0, 0, 0, 0])).is_empty());
    }

    #[test]
    fn hsv_matches_pillow() {
        // Values from Pillow: Image.new("RGB", (1, 1), c).convert("HSV").getpixel((0, 0))
        assert_eq!(pil_hsv(230, 110, 190), (226, 133, 230));
        assert_eq!(pil_hsv(255, 0, 0), (0, 255, 255));
        assert_eq!(pil_hsv(0, 0, 255), (170, 255, 255));
        assert_eq!(pil_hsv(36, 42, 8), (50, 206, 42)); // needs C's double promotion
    }

    #[test]
    fn json_like_python() {
        assert_eq!(colors_json(&[("black", 0.134), ("gray", 1.0)]), r#"{"black": 0.134, "gray": 1.0}"#);
    }
}

