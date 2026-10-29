//! Native features the desktop app provides to the API. The plain server has none.

use std::path::{Path, PathBuf};

pub trait Desktop: Send + Sync {
    /// Show a native folder picker; `None` if cancelled.
    fn pick_folder(&self) -> Option<PathBuf>;
    /// Start a native drag of `files` out of the window, showing `icon_png`.
    fn start_drag(&self, files: Vec<PathBuf>, icon_png: Vec<u8>) -> anyhow::Result<()>;
}

/// A small PNG of a thumbnail for the drag cursor.
pub fn drag_icon(thumb: &Path) -> anyhow::Result<Vec<u8>> {
    let img = image::open(thumb)?.thumbnail(96, 96);
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}
