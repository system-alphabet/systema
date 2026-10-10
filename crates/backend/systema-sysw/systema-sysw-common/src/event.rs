//! Control-port event classification shared by System Wrapper flavors.
//!
//! System A pushes lifecycle envelopes to every control session with one of
//! these well-known methods.  Flavors classify the envelope here and then
//! maintain their memory-mirror of unit state from the payloads.

use sysa::proto::Envelope;

/// The well-known event methods System A pushes to control sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEventKind {
    /// Full `UnitSnapshot`; the unit's configuration or active state changed.
    UnitChanged,
    /// Full `UnitSnapshot`; a unit was loaded into memory.
    UnitNew,
    /// `UnitRemovedEvent`; a unit was unloaded / removed from memory.
    UnitRemoved,
    /// Raw `UnitCgroupMetrics`.
    UnitMetrics,
    /// `JobEvent`; a job was newly enqueued.
    JobNew,
    /// `JobEvent`; a job reached a terminal state (`result` is set).
    JobCompleted,
}

impl ControlEventKind {
    /// The envelope method string for this event kind.
    pub fn method(&self) -> &'static str {
        match self {
            ControlEventKind::UnitChanged => "unit.changed",
            ControlEventKind::UnitNew => "unit.new",
            ControlEventKind::UnitRemoved => "unit.removed",
            ControlEventKind::UnitMetrics => "unit.metrics",
            ControlEventKind::JobNew => "job.new",
            ControlEventKind::JobCompleted => "job.completed",
        }
    }

    /// Classify an envelope method string, or `None` if it is not a control
    /// event (e.g. a reply to one of our own requests).
    pub fn from_method(method: &str) -> Option<ControlEventKind> {
        match method {
            "unit.changed" => Some(ControlEventKind::UnitChanged),
            "unit.new" => Some(ControlEventKind::UnitNew),
            "unit.removed" => Some(ControlEventKind::UnitRemoved),
            "unit.metrics" => Some(ControlEventKind::UnitMetrics),
            "job.new" => Some(ControlEventKind::JobNew),
            "job.completed" => Some(ControlEventKind::JobCompleted),
            _ => None,
        }
    }

    /// Convenience predicate for the job terminal state.
    pub fn is_job_completed(&self) -> bool {
        matches!(self, ControlEventKind::JobCompleted)
    }
}

/// One control-port event: the classified kind plus the raw envelope.
#[derive(Debug)]
pub struct ControlEvent {
    pub kind: ControlEventKind,
    pub envelope: Envelope,
}