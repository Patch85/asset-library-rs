//! Background pipeline run after scans and at startup: thumbnails, then analysis.
//!
//! Each step marks itself running before the previous one clears, and `start` marks the
//! first step running before it returns, so pollers never see an idle gap mid-pipeline.
//! Settings are read at the start of each run.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::analysis::Analysis;
use crate::db::Database;
use crate::power::{lower_current_thread_priority, on_battery};
use crate::settings::SettingsStore;
use crate::thumbs::Thumbs;

const TRAIN_DELAY: Duration = Duration::from_secs(3);

pub struct Jobs {
    db: Arc<Database>,
    pub thumbs: Arc<Thumbs>,
    pub analysis: Arc<Analysis>,
    pub settings: Arc<SettingsStore>,
    state: Mutex<State>,
    paused: AtomicBool,
    battery: Mutex<Option<(Instant, bool)>>,
    /// Classifier retraining: wanted since the last run, and whether a trainer thread is up.
    train_wanted: AtomicBool,
    training: AtomicBool,
}

#[derive(Default)]
struct State {
    active: bool,
    rerun: bool,
}

impl Jobs {
    pub fn new(db: Arc<Database>, thumbs: Arc<Thumbs>, analysis: Arc<Analysis>, settings: Arc<SettingsStore>) -> Self {
        Self {
            db,
            thumbs,
            analysis,
            settings,
            state: Mutex::default(),
            paused: AtomicBool::new(false),
            battery: Mutex::default(),
            train_wanted: AtomicBool::new(false),
            training: AtomicBool::new(false),
        }
    }

    /// Whether background work is currently waiting for mains power.
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Queue a pipeline run. If one is running it will loop once more when done.
    pub fn start(self: &Arc<Self>) {
        self.analysis.index.invalidate(); // assets may have been added/removed
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.active {
            st.rerun = true;
            return;
        }
        st.active = true;
        self.thumbs.status().running = true;
        drop(st);
        let this = Arc::clone(self);
        std::thread::spawn(move || this.run());
    }

    /// Retrain tag classifiers a few seconds after the last call (edits come in bursts).
    /// Only asset types whose tags or embeddings changed are retrained.
    pub fn train_soon(self: &Arc<Self>) {
        self.train_wanted.store(true, Ordering::Relaxed);
        if self.training.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        std::thread::spawn(move || {
            if this.settings.get().low_priority {
                lower_current_thread_priority();
            }
            loop {
                std::thread::sleep(TRAIN_DELAY);
                if this.train_wanted.swap(false, Ordering::AcqRel) {
                    this.wait_for_power();
                    let result = this.analysis.index.get(&this.db).and_then(|idx| this.analysis.classifiers.retrain(&this.db, &idx));
                    if let Err(e) = result {
                        eprintln!("training tag classifiers: {e}");
                    }
                    continue;
                }
                this.training.store(false, Ordering::Release);
                // A request between the check and the store would otherwise be lost.
                if !this.train_wanted.load(Ordering::Acquire) || this.training.swap(true, Ordering::AcqRel) {
                    return;
                }
            }
        });
    }

    fn run(self: &Arc<Self>) {
        loop {
            let s = self.settings.get();
            if s.low_priority {
                lower_current_thread_priority();
            }
            self.analysis.batch.store(s.batch, Ordering::Relaxed);
            self.analysis.threads.store(s.threads, Ordering::Relaxed);
            let low = s.low_priority;
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(s.threads)
                .start_handler(move |_| {
                    if low {
                        lower_current_thread_priority();
                    }
                })
                .build()
                .ok();
            let wait = || self.wait_for_power();

            self.thumbs.warm_all(&self.db, pool.as_ref(), &wait);
            self.analysis.status().running = true;
            self.thumbs.status().running = false;

            self.analysis.analyze_pending(&wait);

            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.rerun {
                st.rerun = false;
                self.thumbs.status().running = true;
                self.analysis.status().running = false;
                continue;
            }
            st.active = false;
            self.analysis.status().running = false;
            drop(st);
            self.train_soon(); // new embeddings or tags
            return;
        }
    }

    /// Block while on battery, if the user asked to pause then.
    fn wait_for_power(&self) {
        while self.settings.get().pause_on_battery && self.on_battery_cached() {
            self.paused.store(true, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(5));
        }
        self.paused.store(false, Ordering::Relaxed);
    }

    /// `on_battery` is a process spawn on macOS, so check at most every few seconds.
    fn on_battery_cached(&self) -> bool {
        let mut cache = self.battery.lock().unwrap_or_else(|e| e.into_inner());
        match *cache {
            Some((at, v)) if at.elapsed() < Duration::from_secs(5) => v,
            _ => {
                let v = on_battery();
                *cache = Some((Instant::now(), v));
                v
            }
        }
    }
}
