use std::path::PathBuf;

pub const IMAGE_EXTENSIONS: &[&str] = &["png", "webp", "jpg", "jpeg", "gif", "bmp"];

/// Asset types the UI offers. New types need no schema changes.
pub const ASSET_TYPES: &[&str] = &["token", "map", "tile", "prop", "texture", "portrait", "other"];

/// Named colours offered by `color:` search, with the swatch the UI shows.
pub const COLOR_HEX: &[(&str, &str)] = &[
    ("red", "#d23c3c"),
    ("orange", "#e8892a"),
    ("yellow", "#e6cf3a"),
    ("green", "#4fa64a"),
    ("teal", "#2fb0a8"),
    ("blue", "#3d6fd6"),
    ("purple", "#8a4fd0"),
    ("pink", "#e05fae"),
    ("brown", "#8a5a32"),
    ("black", "#151515"),
    ("white", "#f2f2f2"),
    ("gray", "#8c8c8c"),
];

pub struct Config {
    pub data_dir: PathBuf,
    pub model_dir: PathBuf,
    /// `ASSET_LIBRARY_AI=0` limits analysis to colours.
    pub ai_enabled: bool,
}

impl Config {
    pub fn from_env() -> Self {
        // TODO(M7): default to the per-user data dir instead of ./data.
        let data_dir = std::env::var_os("ASSET_LIBRARY_DATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data"));
        let model_dir = std::env::var_os("ASSET_LIBRARY_MODELS")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("models"));
        let ai_enabled = std::env::var("ASSET_LIBRARY_AI").map_or(true, |v| v != "0");
        Self { data_dir, model_dir, ai_enabled }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("library.db")
    }
}
