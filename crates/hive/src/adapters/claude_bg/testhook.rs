// --------------------------------------------------------------------------
// test seams: what a hooked_* function in claude_bg reads before falling
// through to the real thing
// --------------------------------------------------------------------------
use super::EngineSession;
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Stand-in for the `claude attach` client: records what was written.
/// Each `text_since` poll drains the next scripted screen frame.
#[derive(Clone, Default)]
pub struct FakePipe {
    pub state: Arc<Mutex<FakeState>>,
}

#[derive(Default)]
pub struct FakeState {
    pub writes: Vec<String>,
    pub stream: String,
    pub pending: VecDeque<String>,
    pub broken_after: Option<usize>,
    pub closed: bool,
    pub killed: bool,
    pub hang_wait: bool,
    pub poll: Option<i32>,
    /// The pid the fake client claims; None reads as a pid nothing runs under.
    pub pid: Option<i32>,
}

impl FakePipe {
    pub fn pid(&self) -> i32 {
        self.state.lock().unwrap().pid.unwrap_or(4242)
    }

    pub fn mark(&self) -> usize {
        self.state.lock().unwrap().stream.chars().count()
    }

    pub fn text_since(&self, mark: usize) -> String {
        let mut st = self.state.lock().unwrap();
        if let Some(frame) = st.pending.pop_front() {
            st.stream.push_str(&frame);
        }
        st.stream.chars().skip(mark).collect()
    }

    pub fn write_str(&self, payload: &str) -> std::io::Result<()> {
        let mut st = self.state.lock().unwrap();
        if let Some(after) = st.broken_after {
            if st.writes.len() >= after {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "client gone",
                ));
            }
        }
        st.writes.push(payload.to_string());
        Ok(())
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
    }

    pub fn poll(&self) -> Option<i32> {
        self.state.lock().unwrap().poll
    }

    pub fn wait_timeout(&self) -> Option<i32> {
        if self.state.lock().unwrap().hang_wait {
            None
        } else {
            Some(0)
        }
    }

    pub fn kill(&self) {
        self.state.lock().unwrap().killed = true;
    }
}

#[derive(Default)]
pub struct Hook {
    pub attach_pipe: Option<FakePipe>,
    pub client_ready: Option<bool>,
    pub wait_engine_behind: Option<Option<EngineSession>>,
    /// Pop per call; the last value repeats (one element for a constant
    /// answer, more for a scripted sequence).
    pub engine_for_job: Option<VecDeque<Option<EngineSession>>>,
    pub forbid_engine_lookup: bool,
    pub transcript_cursor: Option<(Option<PathBuf>, u64)>,
    pub composer_draft: Option<bool>,
    pub pane_for_job: Option<Option<String>>,
    /// Ok((job_id, certainty)) or Err(()) for "the probe blew up".
    pub view_probe: Option<Result<(String, String), ()>>,
    pub suspected_draft: Option<bool>,
    pub suspected_calls: Vec<(String, String)>,
    pub wake_result: Option<bool>,
    pub wakes: Vec<String>,
    pub rename_result: Option<bool>,
    pub renames: Vec<(String, String, String)>,
    pub list_jobs_rows: Option<Option<Vec<Map<String, Value>>>>,
    pub no_sleep: bool,
    pub engine_ready_timeout: Option<f64>,
    pub client_ready_timeout: Option<f64>,
    pub type_retry_after: Option<f64>,
    pub type_ready_timeout: Option<f64>,
    pub slash_confirm_timeout: Option<f64>,
    pub submit_confirm_timeout: Option<f64>,
    pub interrupt_confirm_timeout: Option<f64>,
    pub rename_confirm_timeout: Option<f64>,
    pub rename_poll_interval: Option<f64>,
}

thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

pub fn with<T>(f: impl FnOnce(&mut Hook) -> T) -> Option<T> {
    HOOK.with(|h| h.borrow_mut().as_mut().map(f))
}

pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        HOOK.with(|h| *h.borrow_mut() = None);
    }
}

pub fn install(hook: Hook) -> Guard {
    HOOK.with(|h| *h.borrow_mut() = Some(hook));
    Guard
}
