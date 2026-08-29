//! Runtime snapshot primitives used by the hived.
//!
//! Current scope is sessionId-only. This keeps the owner boundary narrow: the
//! hived maintains current-session identity, and snapshot-only consumers such
//! as cvim read that identity without launching their own live probes.
//!
//! Stale snapshots may retain the last observed value for diagnostics, but
//! consumers must treat `RuntimeField::is_fresh()` / `_sessionIdFresh` as the
//! authority on whether that value can be used.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use serde_json::{Map, Value};

/// Monotonic seconds with a process-wide origin, mirroring `time.monotonic()`.
fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeField {
    pub value: String,
    pub source: String,
    pub observed_at: f64,
    pub generation: u64,
    pub freshness_s: Option<f64>,
}

impl RuntimeField {
    pub fn is_fresh(&self, now: Option<f64>) -> bool {
        match self.freshness_s {
            None => true,
            Some(freshness_s) => (now.unwrap_or_else(monotonic) - self.observed_at) <= freshness_s,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
#[allow(non_snake_case)]
pub struct RuntimeSnapshot {
    pub pane_id: String,
    pub generation: u64,
    pub sessionId: RuntimeField,
}

impl RuntimeSnapshot {
    pub fn to_runtime_fields(&self, now: Option<f64>) -> Map<String, Value> {
        let session = &self.sessionId;
        let mut payload = Map::new();
        payload.insert("sessionId".to_string(), Value::from(session.value.clone()));
        payload.insert(
            "_sessionIdSource".to_string(),
            Value::from(session.source.clone()),
        );
        payload.insert(
            "_runtimeGeneration".to_string(),
            Value::from(self.generation),
        );
        payload.insert(
            "_sessionIdObservedAt".to_string(),
            Value::from(session.observed_at),
        );
        payload.insert(
            "_sessionIdFresh".to_string(),
            Value::from(session.is_fresh(now)),
        );
        if let Some(freshness_s) = session.freshness_s {
            payload.insert("_sessionIdFreshnessS".to_string(), Value::from(freshness_s));
        }
        payload
    }
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeSnapshotStore {
    pub snapshots: HashMap<String, RuntimeSnapshot>,
    pub generation: u64,
}

impl RuntimeSnapshotStore {
    pub fn get(&self, pane_id: &str) -> Option<&RuntimeSnapshot> {
        self.snapshots.get(pane_id)
    }

    pub fn update_session_id(
        &mut self,
        pane_id: &str,
        session_id: &str,
        source: &str,
        observed_at: Option<f64>,
        freshness_s: Option<f64>,
    ) -> RuntimeSnapshot {
        self.generation += 1;
        let generation = self.generation;
        let field = RuntimeField {
            value: session_id.to_string(),
            source: source.to_string(),
            observed_at: observed_at.unwrap_or_else(monotonic),
            generation,
            freshness_s,
        };
        let snapshot = RuntimeSnapshot {
            pane_id: pane_id.to_string(),
            generation,
            sessionId: field,
        };
        self.snapshots.insert(pane_id.to_string(), snapshot.clone());
        snapshot
    }

    pub fn clear(&mut self) {
        self.snapshots.clear();
        self.generation = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_snapshot_store_updates_session_generation() {
        let mut store = RuntimeSnapshotStore::default();

        let first = store.update_session_id("%1", "sid-a", "fd", Some(10.0), None);
        let second = store.update_session_id("%1", "sid-b", "fd", Some(11.0), None);

        assert_eq!(first.generation, 1);
        assert_eq!(first.sessionId.generation, 1);
        assert_eq!(second.generation, 2);
        assert_eq!(second.sessionId.value, "sid-b");
        assert_eq!(store.get("%1"), Some(&second));
    }

    #[test]
    fn test_runtime_field_freshness() {
        let mut store = RuntimeSnapshotStore::default();
        let snapshot = store.update_session_id("%1", "sid-a", "fd", Some(10.0), Some(5.0));

        assert!(snapshot.sessionId.is_fresh(Some(14.0)));
        assert!(!snapshot.sessionId.is_fresh(Some(16.0)));
        assert_eq!(
            snapshot.to_runtime_fields(Some(16.0))["_sessionIdFresh"],
            Value::Bool(false)
        );
        assert_eq!(
            snapshot.to_runtime_fields(Some(16.0))["_sessionIdFreshnessS"],
            Value::from(5.0)
        );
    }
}
