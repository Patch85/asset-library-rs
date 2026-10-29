//! Background pipeline run after scans and at startup: thumbnails, then analysis.
//!
//! Each step marks itself running before the previous one clears, and `start` marks the
//! first step running before it returns, so pollers never see an idle gap mid-pipeline.

use std::sync::{Arc, Mutex};

use crate::analysis::Analysis;
use crate::db::Database;
use crate::thumbs::{Thumbs, default_threads};

pub struct Jobs {
    db: Arc<Database>,
    pub thumbs: Arc<Thumbs>,
    pub analysis: Arc<Analysis>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    active: bool,
    rerun: bool,
}

impl Jobs {
    pub fn new(db: Arc<Database>, thumbs: Arc<Thumbs>, analysis: Arc<Analysis>) -> Self {
        Self { db, thumbs, analysis, state: Mutex::default() }
    }

    /// Queue a pipeline run. If one is running it will loop once more when done.
    pub fn start(self: &Arc<Self>) {
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

    fn run(&self) {
        loop {
            self.thumbs.warm_all(&self.db, default_threads());
            self.analysis.status().running = true;
            self.thumbs.status().running = false;

            self.analysis.analyze_pending();

            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.rerun {
                st.rerun = false;
                self.thumbs.status().running = true;
                self.analysis.status().running = false;
                continue;
            }
            st.active = false;
            self.analysis.status().running = false;
            return;
        }
    }
}
