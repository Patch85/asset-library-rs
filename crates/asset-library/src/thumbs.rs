//! Cached WebP thumbnails keyed by content hash (so moved files keep theirs), as in the
//! original's `app/thumbs.py`. Stored at `thumbs/<hash[:2]>/<hash>.webp`.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, anyhow};
use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, ImageDecoder, ImageReader};
use rayon::prelude::*;
use serde::Serialize;

use crate::db::Database;
use crate::scanner::join_rel;

pub const THUMB_SIZE: u32 = 256;
const QUALITY: f32 = 85.0;

#[derive(Clone, Default, Serialize)]
pub struct WarmStatus {
    pub running: bool,
    pub total: u64,
    pub done: u64,
    pub failed: u64,
}

pub struct Thumbs {
    dir: PathBuf,
    status: Mutex<WarmStatus>,
}

impl Thumbs {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, status: Mutex::default() }
    }

    pub fn status(&self) -> MutexGuard<'_, WarmStatus> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn path(&self, file_hash: &str) -> PathBuf {
        let prefix = file_hash.get(..2).unwrap_or(file_hash);
        self.dir.join(prefix).join(format!("{file_hash}.webp"))
    }

    /// The thumbnail for `source`, rendering it first if it isn't cached.
    pub fn get(&self, source: &Path, file_hash: &str) -> anyhow::Result<PathBuf> {
        let out = self.path(file_hash);
        if out.exists() {
            return Ok(out);
        }
        let dir = out.parent().context("thumbnail path has no parent")?;
        std::fs::create_dir_all(dir)?;
        let webp = catch_unwind(AssertUnwindSafe(|| render(source)))
            .map_err(|_| anyhow!("decoder panicked on {}", source.display()))??;
        // Unique temp name: the browser and the warmer may build the same thumb at once.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!("{file_hash}.{}-{n}.tmp", std::process::id()));
        std::fs::write(&tmp, webp)?;
        if let Err(e) = std::fs::rename(&tmp, &out) {
            // Another thread won the race (Windows can't replace an open file).
            let _ = std::fs::remove_file(&tmp);
            if !out.exists() {
                return Err(e.into());
            }
        }
        Ok(out)
    }

    /// Generate every missing thumbnail on `threads` worker threads. The caller owns the
    /// `running` flag, so the pipeline can hand over to analysis without an idle gap.
    pub fn warm_all(&self, db: &Database, threads: usize) {
        let rows: Vec<(String, PathBuf)> = db
            .tx(|c| {
                let mut stmt = c.prepare(
                    "SELECT a.file_hash, s.path, a.rel_path FROM assets a
                     JOIN sources s ON s.id = a.source_id
                     WHERE a.missing = 0 ORDER BY a.name COLLATE NOCASE", // matches default sort
                )?;
                let rows = stmt
                    .query_map([], |r| {
                        let root: String = r.get(1)?;
                        let rel: String = r.get(2)?;
                        Ok((r.get(0)?, join_rel(Path::new(&root), &rel)))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>();
                rows
            })
            .unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        let todo: Vec<(String, PathBuf)> = rows
            .into_iter()
            .filter(|(h, _)| !self.path(h).exists() && seen.insert(h.clone()))
            .collect();
        {
            let mut st = self.status();
            *st = WarmStatus { running: st.running, total: todo.len() as u64, ..Default::default() };
        }
        if todo.is_empty() {
            return;
        }
        let one = |(hash, path): &(String, PathBuf)| {
            let ok = self.get(path, hash).is_ok();
            let mut st = self.status();
            st.failed += u64::from(!ok);
            st.done += 1;
        };
        match rayon::ThreadPoolBuilder::new().num_threads(threads.max(1)).build() {
            Ok(pool) => pool.install(|| todo.par_iter().for_each(one)),
            Err(_) => todo.iter().for_each(one),
        }
    }
}

/// Worker threads for background work: leave a core free so the UI stays responsive.
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1).max(1))
}

/// Decode the first frame, apply EXIF orientation, fit within THUMB_SIZE and encode as
/// lossy WebP, as Pillow's `thumbnail()` + `save(quality=85)` do.
pub fn render(source: &Path) -> anyhow::Result<Vec<u8>> {
    let mut decoder = ImageReader::open(source)?.with_guessed_format()?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    let img = match img {
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_) => img,
        i if i.color().has_alpha() || !i.color().has_color() => i.to_rgba8().into(),
        i => i.to_rgb8().into(),
    };
    let (w, h) = thumb_size(img.width(), img.height(), THUMB_SIZE);
    let img = if (w, h) == (img.width(), img.height()) {
        img
    } else {
        let mut dst = DynamicImage::new(w, h, img.color());
        let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
        Resizer::new().resize(&img, &mut dst, &opts)?;
        dst
    };
    let encoder = webp::Encoder::from_image(&img).map_err(|e| anyhow!("webp: {e}"))?;
    Ok(encoder.encode(QUALITY).to_vec())
}

/// Pillow's `Image.thumbnail` size: fit within `max`×`max`, keep the aspect ratio by
/// picking whichever of floor/ceil is closer to it, and never upscale.
fn thumb_size(width: u32, height: u32, max: u32) -> (u32, u32) {
    if max >= width && max >= height {
        return (width, height);
    }
    let aspect = width as f64 / height as f64;
    let (x, y) = (max as f64, max as f64);
    let round = |n: f64, key: &dyn Fn(f64) -> f64| -> u32 {
        let (lo, hi) = (n.floor(), n.ceil());
        // Python's min() keeps the first of equal keys.
        let best = if key(hi) < key(lo) { hi } else { lo };
        (best as u32).max(1)
    };
    if x / y >= aspect {
        (round(y * aspect, &|n| (aspect - n / y).abs()), max)
    } else {
        (max, round(x / aspect, &|n| if n == 0.0 { 0.0 } else { (aspect - x / n).abs() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_sizes_match_pillow() {
        assert_eq!(thumb_size(8, 8, 256), (8, 8));
        assert_eq!(thumb_size(512, 512, 256), (256, 256));
        assert_eq!(thumb_size(1000, 333, 256), (256, 85));
        assert_eq!(thumb_size(333, 1000, 256), (85, 256));
        assert_eq!(thumb_size(5000, 3, 256), (256, 1));
        assert_eq!(thumb_size(300, 200, 256), (256, 171));
    }

    #[test]
    fn renders_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.png");
        image::RgbaImage::from_pixel(600, 300, image::Rgba([200, 0, 0, 128])).save(&src).unwrap();
        let thumbs = Thumbs::new(dir.path().join("thumbs"));
        let out = thumbs.get(&src, "abcdef").unwrap();
        assert!(out.ends_with("ab/abcdef.webp"));
        let img = image::open(&out).unwrap();
        assert_eq!((img.width(), img.height()), (256, 128));
        assert!(img.color().has_alpha());
    }
}
