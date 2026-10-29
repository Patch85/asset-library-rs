//! CLIP image and text embeddings (fastembed, ONNX), loaded lazily from the model cache.
//! The cache uses the Hugging Face layout, so it is shared with the Python app's.
//!
//! ONNX Runtime is loaded at runtime from the official Microsoft build (see
//! tools/fetch_onnxruntime.py), which runs on older glibc than ort's static library.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::anyhow;
use fastembed::{EmbeddingModel, ImageEmbedding, ImageEmbeddingModel, ImageInitOptions, TextEmbedding, TextInitOptions};
use image::{DynamicImage, RgbImage};

pub const IMAGE_MODEL: &str = "Qdrant/clip-ViT-B-32-vision";
pub const EMBED_DIM: usize = 512;

#[cfg(windows)]
const ORT_LIB: &str = "onnxruntime.dll";
#[cfg(target_os = "macos")]
const ORT_LIB: &str = "libonnxruntime.dylib";
#[cfg(not(any(windows, target_os = "macos")))]
const ORT_LIB: &str = "libonnxruntime.so";

/// tools/fetch_onnxruntime.py's folder name for this platform.
const VENDOR_PLATFORM: &str = if cfg!(windows) {
    "win-x64"
} else if cfg!(target_os = "macos") {
    "osx-arm64"
} else if cfg!(target_arch = "aarch64") {
    "linux-aarch64"
} else {
    "linux-x64"
};

/// Where to look for ONNX Runtime: `ORT_DYLIB_PATH`, then `extra_dirs`, then next to the
/// executable (and `lib/` beside it), then, in debug builds, the repo's vendor directory.
fn runtime_candidates(extra_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::env::var_os("ORT_DYLIB_PATH").map(PathBuf::from).into_iter().collect();
    out.extend(extra_dirs.iter().map(|d| d.join(ORT_LIB)));
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        out.push(exe_dir.join(ORT_LIB));
        out.push(exe_dir.join("lib").join(ORT_LIB));
        out.push(exe_dir.join("../Frameworks").join(ORT_LIB)); // macOS app bundle
    }
    if cfg!(debug_assertions) {
        let vendor = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/onnxruntime");
        out.push(vendor.join(VENDOR_PLATFORM).join(ORT_LIB));
    }
    out
}

/// Load ONNX Runtime once per process.
fn load_runtime(extra_dirs: &[PathBuf]) -> anyhow::Result<()> {
    static LOADED: OnceLock<Result<(), String>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            let candidates = runtime_candidates(extra_dirs);
            let path = candidates.iter().find(|p| p.exists()).ok_or_else(|| {
                format!("ONNX Runtime ({ORT_LIB}) not found; looked in {candidates:?}")
            })?;
            ort::init_from(path).map(drop).map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| anyhow!(e))
}

pub struct Clip {
    model_dir: PathBuf,
    runtime_dirs: Vec<PathBuf>,
    image: Mutex<Option<ImageEmbedding>>,
    text: Mutex<Option<TextEmbedding>>,
}

impl Clip {
    pub fn new(model_dir: PathBuf) -> Self {
        Self { model_dir, runtime_dirs: Vec::new(), image: Mutex::default(), text: Mutex::default() }
    }

    /// Also look for ONNX Runtime in `dir` (e.g. the desktop app's resource folder).
    pub fn with_runtime_dir(mut self, dir: PathBuf) -> Self {
        self.runtime_dirs.push(dir);
        self
    }

    /// Whether the image model is in the cache (otherwise loading it downloads it).
    pub fn image_model_cached(&self) -> bool {
        self.model_dir.join("models--Qdrant--clip-ViT-B-32-vision").join("snapshots").is_dir()
    }

    pub fn image_model_loaded(&self) -> bool {
        self.image.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// L2-normalized embeddings of images composited onto neutral gray.
    pub fn embed_images(&self, images: Vec<DynamicImage>, threads: usize) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut guard = self.image.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            load_runtime(&self.runtime_dirs)?;
            std::fs::create_dir_all(&self.model_dir)?;
            let opts = ImageInitOptions::new(ImageEmbeddingModel::ClipVitB32)
                .with_cache_dir(self.model_dir.clone())
                .with_show_download_progress(false)
                .with_intra_threads(threads.max(1));
            *guard = Some(ImageEmbedding::try_new(opts).map_err(|e| anyhow!("{e}"))?);
        }
        let model = guard.as_mut().expect("loaded above");
        let images = images.iter().map(|im| on_gray(im).into()).collect();
        let vecs = model.embed_images(images).map_err(|e| anyhow!("{e}"))?;
        Ok(vecs.into_iter().map(normalize).collect())
    }

    /// Free the image model (about 350 MB) between analysis runs.
    pub fn release_image_model(&self) {
        *self.image.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn embed_text(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut guard = self.text.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            load_runtime(&self.runtime_dirs)?;
            std::fs::create_dir_all(&self.model_dir)?;
            let opts = TextInitOptions::new(EmbeddingModel::ClipVitB32)
                .with_cache_dir(self.model_dir.clone())
                .with_show_download_progress(false);
            *guard = Some(TextEmbedding::try_new(opts).map_err(|e| anyhow!("{e}"))?);
        }
        let model = guard.as_mut().expect("loaded above");
        let mut vecs = model.embed([text], None).map_err(|e| anyhow!("{e}"))?;
        vecs.pop().map(normalize).ok_or_else(|| anyhow!("no text embedding"))
    }
}

pub fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

/// CLIP ignores alpha; composite tokens onto neutral gray instead of black.
pub fn on_gray(image: &DynamicImage) -> RgbImage {
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba.get_pixel(x, y);
        let a = p[3] as u32;
        let mix = |c: u8| ((c as u32 * a + 128 * (255 - a) + 127) / 255) as u8;
        image::Rgb([mix(p[0]), mix(p[1]), mix(p[2])])
    })
}

pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn from_blob(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}
