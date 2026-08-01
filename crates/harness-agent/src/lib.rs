//! The sample↔tools agent loop, with compaction, cost-aware escalation off
//! the cheap default, and optional budget enforcement. Analogue of
//! grok-build's session driver (`xai-grok-shell`) + `xai-grok-agent`'s
//! policy bundle.

mod agent;
mod checkpoint;
mod compaction;
mod cost;
mod hooks;
mod interjection;
mod session;
mod tokens;
mod trim;
mod ui;

pub use agent::Agent;
pub use checkpoint::UndoReport;
pub use compaction::{CompactionPolicy, CompactionReport};
pub use cost::estimate_cost_usd;
pub use interjection::InterjectionQueue;
pub use session::{
    SessionError, SessionRecord, SessionStore, SessionSummary, derive_title, unix_now,
};
pub use tokens::estimate_tokens;
pub use ui::Ui;
