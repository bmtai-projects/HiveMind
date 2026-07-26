//! The sample↔tools agent loop, with compaction, cost-aware escalation off
//! the cheap default, and optional budget enforcement. Analogue of
//! grok-build's session driver (`xai-grok-shell`) + `xai-grok-agent`'s
//! policy bundle.

mod agent;
mod checkpoint;
mod compaction;
mod cost;
mod hooks;
mod ui;

pub use agent::Agent;
pub use checkpoint::UndoReport;
pub use compaction::{CompactionPolicy, CompactionReport};
pub use cost::estimate_cost_usd;
pub use ui::Ui;
