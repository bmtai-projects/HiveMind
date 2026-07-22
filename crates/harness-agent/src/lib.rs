//! The sample↔tools agent loop, with compaction and cost-aware Flash→Pro
//! escalation. Analogue of grok-build's session driver
//! (`xai-grok-shell`) + `xai-grok-agent`'s policy bundle, scoped to
//! DeepSeek's two tiers.

mod agent;
mod checkpoint;
mod compaction;
mod ui;

pub use agent::Agent;
pub use checkpoint::UndoReport;
pub use compaction::{CompactionPolicy, CompactionReport};
pub use ui::Ui;
