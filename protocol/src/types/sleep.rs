//! Ночь сна: запуски и их фазы.

use eva::{data, time::Timestamp};
use serde_json::Value;

use crate::types::{ModelId, RunId};

#[data]
pub struct Run {
    pub run_id: RunId,
    pub started: Timestamp,
    pub finished: Option<Timestamp>,
    pub ok: Option<bool>,
    pub dry_run: bool,
    pub interrupted: bool,
    pub report: Option<String>,
}

#[data(copy, ord)]
pub enum Phase {
    Memory,
    Groups,
    Warm,
}

#[data]
pub struct PhaseRun {
    pub run: RunId,
    pub phase: Phase,
    pub started: Timestamp,
    pub finished: Option<Timestamp>,
    pub model: Option<ModelId>,
    pub ops: Vec<PhaseOp>,
    pub applied: u32,
    pub discarded: u32,
    pub report: Option<String>,
}

#[data]
pub struct PhaseOp {
    pub tool: String,
    pub input: Value,
    pub verdict: Verdict,
}

#[data]
pub enum Verdict {
    Applied,
    /// `dry_run`: применилось бы.
    WouldApply,
    Discarded {
        reason: String,
    },
}
