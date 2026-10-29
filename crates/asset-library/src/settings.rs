//! User settings, stored as `settings.json` in the data dir.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::thumbs::default_threads;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// CLIP image search, look-alike suggestions. Downloads about 600 MB once.
    pub ai: bool,
    /// Worker threads for thumbnails and CLIP.
    pub threads: usize,
    /// Images per CLIP batch; smaller uses less memory.
    pub batch: usize,
    /// Run background work below normal priority.
    pub low_priority: bool,
    /// Pause background work while on battery.
    pub pause_on_battery: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { ai: false, threads: default_threads(), batch: 64, low_priority: true, pause_on_battery: true }
    }
}

impl Settings {
    fn clamp(mut self) -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        self.threads = self.threads.clamp(1, cpus);
        self.batch = self.batch.clamp(1, 256);
        self
    }
}

pub struct SettingsStore {
    path: PathBuf,
    current: Mutex<Settings>,
    /// From `ASSET_LIBRARY_AI`; wins over the stored value without being saved.
    pub ai_override: Option<bool>,
}

impl SettingsStore {
    /// Load `settings.json`, or start from `defaults` if it's missing or unreadable.
    pub fn load(path: PathBuf, defaults: Settings) -> Self {
        let current = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
            .unwrap_or(defaults)
            .clamp();
        Self { path, current: Mutex::new(current), ai_override: None }
    }

    pub fn get(&self) -> Settings {
        let mut s = self.lock().clone();
        s.ai = self.ai_override.unwrap_or(s.ai);
        s
    }

    /// Replace the settings and save them; returns what was stored (after clamping).
    pub fn set(&self, s: Settings) -> std::io::Result<Settings> {
        let mut s = s.clamp();
        if self.ai_override.is_some() {
            s.ai = self.lock().ai; // the override isn't the user's choice; don't save it
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(&s).unwrap_or_default())?;
        *self.lock() = s.clone();
        Ok(self.get())
    }

    fn lock(&self) -> MutexGuard<'_, Settings> {
        self.current.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_fills_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"ai": true, "batch": 9999}"#).unwrap();
        let store = SettingsStore::load(path.clone(), Settings::default());
        let s = store.get();
        assert!(s.ai && s.low_priority);
        assert_eq!(s.batch, 256);
        store.set(Settings { ai: false, ..s }).unwrap();
        assert!(!SettingsStore::load(path.clone(), Settings::default()).get().ai);

        let mut overridden = SettingsStore::load(path.clone(), Settings::default());
        overridden.ai_override = Some(true);
        assert!(overridden.get().ai);
        overridden.set(Settings { threads: 1, ..overridden.get() }).unwrap();
        let reloaded = SettingsStore::load(path, Settings::default()).get();
        assert!(!reloaded.ai && reloaded.threads == 1);
    }
}
