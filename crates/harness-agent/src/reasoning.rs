//! Which reasoning levels may be offered, and sent, for the model in use.

use harness_provider::RemoteModel;

/// Offered when nothing says what a model accepts. Most models that expose
/// an effort setting use these three.
const COMMON_LEVELS: [&str; 3] = ["low", "medium", "high"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningOptions {
    pub levels: Vec<String>,
    /// True when HiveMind's catalog or the provider named these levels. False
    /// means they are a guess, and the choice is sent as-is.
    pub verified: bool,
}

impl ReasoningOptions {
    /// A model known to have no adjustable reasoning.
    pub fn unsupported(&self) -> bool {
        self.verified && self.levels.is_empty()
    }

    /// Whether `effort` may be sent. A model whose levels are known only gets
    /// levels it accepts; for an unknown model the user's choice goes through.
    pub fn allows(&self, effort: &str) -> bool {
        !self.verified || self.levels.iter().any(|level| level == effort)
    }
}

/// `list_reports_reasoning`: the provider's list described reasoning for at
/// least one model, so a model without it there really has none.
pub(crate) fn options_for(
    model_id: &str,
    listed: &[RemoteModel],
    list_reports_reasoning: bool,
) -> ReasoningOptions {
    let verified = |levels: Vec<String>| ReasoningOptions {
        levels,
        verified: true,
    };
    if let Some(entry) = harness_config::lookup_model(model_id) {
        return verified(
            entry
                .reasoning_efforts
                .iter()
                .map(|level| level.to_string())
                .collect(),
        );
    }
    if let Some(model) = listed.iter().find(|m| m.id == model_id) {
        match &model.reasoning_efforts {
            Some(levels) => return verified(levels.clone()),
            None if list_reports_reasoning => return verified(Vec::new()),
            None => {}
        }
    }
    ReasoningOptions {
        levels: COMMON_LEVELS
            .iter()
            .map(|level| level.to_string())
            .collect(),
        verified: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(id: &str, efforts: Option<&[&str]>) -> RemoteModel {
        RemoteModel {
            id: id.into(),
            reasoning_efforts: efforts.map(|e| e.iter().map(|s| s.to_string()).collect()),
            ..RemoteModel::default()
        }
    }

    #[test]
    fn a_catalog_model_uses_the_catalog_levels() {
        let options = options_for("claude-sonnet-5", &[], false);
        assert!(options.verified);
        assert_eq!(options.levels, ["low", "medium", "high", "xhigh", "max"]);
        assert!(options.allows("max") && !options.allows("minimal"));
    }

    #[test]
    fn a_catalog_model_without_reasoning_is_unsupported() {
        let options = options_for("grok-build", &[], false);
        assert!(options.unsupported());
        assert!(!options.allows("high"));
    }

    #[test]
    fn the_providers_list_names_the_exact_levels_for_an_uncatalogued_model() {
        let models = [listed(
            "deepseek/deepseek-v4-flash",
            Some(&["xhigh", "high", "none"]),
        )];
        let options = options_for("deepseek/deepseek-v4-flash", &models, true);
        assert_eq!(options.levels, ["xhigh", "high", "none"]);
        assert!(options.verified);
        assert!(options.allows("high") && !options.allows("medium"));
    }

    #[test]
    fn a_listed_model_without_reasoning_is_unsupported_when_the_list_describes_reasoning() {
        let models = [
            listed("a/thinker", Some(&["high"])),
            listed("a/plain", None),
        ];
        assert!(options_for("a/plain", &models, true).unsupported());
    }

    #[test]
    fn a_list_that_never_mentions_reasoning_proves_nothing() {
        // Ollama and plain OpenAI-style lists carry ids only.
        let models = [listed("llama3", None)];
        let options = options_for("llama3", &models, false);
        assert!(!options.verified && !options.unsupported());
        assert_eq!(options.levels, ["low", "medium", "high"]);
        assert!(options.allows("high"), "the user's choice is sent as-is");
        assert!(options.allows("anything"));
    }

    #[test]
    fn a_model_the_list_does_not_contain_is_a_guess_too() {
        let models = [listed("a/b", Some(&["high"]))];
        let options = options_for("typed/by-hand", &models, true);
        assert!(!options.verified);
        assert!(options.allows("medium"));
    }
}
