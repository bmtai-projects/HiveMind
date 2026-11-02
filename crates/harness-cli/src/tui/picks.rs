//! Which models to put first when someone has to choose among hundreds.
//!
//! Nothing here is HiveMind's own judgement of quality. Every ranking uses
//! only what the provider publishes about each model: the Artificial Analysis
//! intelligence index (a third-party benchmark), price, context window, release
//! date, and the provider's own list of models people use for programming.
//! Models the provider gives no score are never ranked by score.

use super::events::ModelChoice;

pub(crate) const TOP_COUNT: usize = 5;
/// Below this many scored models a "strongest quarter" means nothing.
const MIN_SCORED: usize = 8;
const MIN_CONTEXT: u64 = 32_000;
/// "Smart & affordable" means at most this many dollars per million tokens,
/// blended.
const AFFORDABLE_BLENDED: f64 = 1.0;

/// Index lists point into the `choices` slice given to [`rank`].
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Ranking {
    /// Up to five starting points, each with the reasons it is listed.
    pub(crate) top: Vec<(usize, Vec<&'static str>)>,
    pub(crate) smartest: Vec<usize>,
    pub(crate) cheapest: Vec<usize>,
    pub(crate) context: Vec<usize>,
    pub(crate) popular: Vec<usize>,
}

impl Ranking {
    fn is_empty(&self) -> bool {
        self.top.is_empty()
            && self.smartest.is_empty()
            && self.cheapest.is_empty()
            && self.context.is_empty()
            && self.popular.is_empty()
    }
}

/// Dollars per million tokens, weighting input three to one over output:
/// an agent re-sends its context every turn, so input dominates a bill.
pub(crate) fn blended_price(choice: &ModelChoice) -> Option<f64> {
    Some(0.75 * choice.input_per_m? + 0.25 * choice.output_per_m?)
}

/// `$` to `$$$$`, for a glance at how expensive a model is.
pub(crate) fn cost_tier(blended: f64) -> &'static str {
    match blended {
        b if b <= 0.25 => "$",
        b if b <= 1.5 => "$$",
        b if b <= 6.0 => "$$$",
        _ => "$$$$",
    }
}

/// Free tiers, batch queues and moving "latest" aliases are not what someone
/// picking a model to work with wants listed first.
fn is_variant(id: &str) -> bool {
    id.ends_with(":free") || id.ends_with(":batch") || id.starts_with('~')
}

fn scored(choice: &ModelChoice) -> Option<f64> {
    let intelligence = choice.intelligence?;
    (choice.supports_tools == Some(true)
        && choice.text_output != Some(false)
        && choice.context_window.is_some_and(|c| c >= MIN_CONTEXT)
        && blended_price(choice).is_some_and(|p| p > 0.0)
        && !is_variant(&choice.id))
    .then_some(intelligence)
}

/// `None` when the provider gave nothing to rank by.
pub(crate) fn rank(choices: &[ModelChoice]) -> Option<Ranking> {
    let mut popular: Vec<usize> = choices
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.popular_rank.is_some()
                && c.supports_tools != Some(false)
                && c.text_output != Some(false)
                && !is_variant(&c.id)
        })
        .map(|(index, _)| index)
        .collect();
    popular.sort_by_key(|&index| choices[index].popular_rank);

    let scores: Vec<(usize, f64)> = choices
        .iter()
        .enumerate()
        .filter_map(|(index, c)| Some((index, scored(c)?)))
        .collect();
    let mut ranking = Ranking {
        popular,
        ..Ranking::default()
    };
    if scores.len() >= MIN_SCORED {
        fill_scored(&mut ranking, choices, &scores);
    }
    (!ranking.is_empty()).then_some(ranking)
}

fn fill_scored(ranking: &mut Ranking, choices: &[ModelChoice], scores: &[(usize, f64)]) {
    let price = |index: usize| blended_price(&choices[index]).unwrap_or(f64::MAX);
    let score_of = |index: usize| scores.iter().find(|s| s.0 == index).map_or(0.0, |s| s.1);
    let by = |mut list: Vec<usize>, key: &dyn Fn(usize) -> f64| {
        list.sort_by(|&a, &b| key(a).total_cmp(&key(b)).then(a.cmp(&b)));
        list
    };

    let mut sorted_scores: Vec<f64> = scores.iter().map(|s| s.1).collect();
    sorted_scores.sort_by(f64::total_cmp);
    let strong_floor = sorted_scores[(sorted_scores.len() * 3) / 4];
    let all: Vec<usize> = scores.iter().map(|s| s.0).collect();
    let strong: Vec<usize> = all
        .iter()
        .copied()
        .filter(|&i| score_of(i) >= strong_floor)
        .collect();

    ranking.smartest = by(all.clone(), &|i| -score_of(i) + price(i) * 1e-9);
    ranking.cheapest = by(strong.clone(), &|i| price(i) - score_of(i) * 1e-9);
    ranking.context = by(strong.clone(), &|i| {
        -(choices[i].context_window.unwrap_or(0) as f64) - score_of(i) * 1e-6
    });
    let value = by(strong.clone(), &|i| -(score_of(i) / price(i).max(0.01)));
    let affordable = by(
        all.iter()
            .copied()
            .filter(|&i| price(i) <= AFFORDABLE_BLENDED)
            .collect(),
        &|i| -score_of(i) + price(i) * 1e-9,
    );
    let newest = by(strong, &|i| -(choices[i].created.unwrap_or(0) as f64));

    let criteria: [(&'static str, &[usize]); 5] = [
        ("Smartest", &ranking.smartest),
        ("Best value", &value),
        ("Smart & affordable", &affordable),
        ("Popular for coding", &ranking.popular),
        ("Newest strong", &newest),
    ];
    let mut top: Vec<(usize, Vec<&'static str>)> = Vec::new();
    for (reason, list) in criteria {
        let Some(&winner) = list.first() else {
            continue;
        };
        match top.iter_mut().find(|(index, _)| *index == winner) {
            Some((_, reasons)) => reasons.push(reason),
            None => top.push((winner, vec![reason])),
        }
    }
    // A model that won several criteria leaves a slot to fill.
    for &index in ranking.popular.iter().chain(&ranking.smartest) {
        if top.len() >= TOP_COUNT {
            break;
        }
        if !top.iter().any(|(i, _)| *i == index) {
            top.push((index, vec!["Strong all-round"]));
        }
    }
    top.truncate(TOP_COUNT);
    ranking.top = top;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model with everything a ranking looks at.
    fn model(
        id: &str,
        intelligence: f64,
        prices: (f64, f64),
        context: u64,
        created: i64,
    ) -> ModelChoice {
        ModelChoice {
            id: id.into(),
            context_window: Some(context),
            input_per_m: Some(prices.0),
            output_per_m: Some(prices.1),
            created: Some(created),
            supports_tools: Some(true),
            text_output: Some(true),
            intelligence: Some(intelligence),
            ..ModelChoice::default()
        }
    }

    /// Twenty scored models. Fifteen weak ones (scores 1 to 15) and five strong
    /// ones, so the strongest quarter is exactly the five at 46 and above.
    ///
    /// | model        | score | blended $ | context | created |
    /// |--------------|-------|-----------|---------|---------|
    /// | big/genius   | 60    | 8.0       | 1M      | 100     |
    /// | big/solid    | 54    | 4.0       | 1M      | 300     |
    /// | mid/fast     | 50    | 0.75      | 256k    | 400     |
    /// | mid/value    | 47    | 0.095     | 512k    | 200     |
    /// | mid/new      | 46    | 1.35      | 1M      | 900     |
    fn market() -> Vec<ModelChoice> {
        let mut choices = vec![
            model("big/genius", 60.0, (4.0, 20.0), 1_000_000, 100),
            model("big/solid", 54.0, (2.0, 10.0), 1_000_000, 300),
            model("mid/fast", 50.0, (0.5, 1.5), 256_000, 400),
            model("mid/value", 47.0, (0.06, 0.2), 512_000, 200),
            model("mid/new", 46.0, (0.8, 3.0), 1_000_000, 900),
        ];
        choices.extend((0..15).map(|i| {
            model(
                &format!("low/{i:02}"),
                1.0 + f64::from(i),
                (0.05, 0.2),
                131_000,
                10 + i64::from(i),
            )
        }));
        choices
    }

    fn ids<'a>(choices: &'a [ModelChoice], list: &[usize]) -> Vec<&'a str> {
        list.iter().map(|&i| choices[i].id.as_str()).collect()
    }

    fn index_of(choices: &[ModelChoice], id: &str) -> usize {
        choices.iter().position(|c| c.id == id).unwrap()
    }

    #[test]
    fn blended_price_weights_input_three_to_one_and_needs_both_prices() {
        let c = model("a/b", 1.0, (4.0, 20.0), 100_000, 0);
        assert!((blended_price(&c).unwrap() - 8.0).abs() < 1e-9);
        let mut half = c.clone();
        half.output_per_m = None;
        assert_eq!(blended_price(&half), None);
    }

    #[test]
    fn cost_tiers_split_at_clear_round_numbers() {
        assert_eq!(cost_tier(0.1), "$");
        assert_eq!(cost_tier(0.25), "$");
        assert_eq!(cost_tier(1.0), "$$");
        assert_eq!(cost_tier(4.0), "$$$");
        assert_eq!(cost_tier(8.0), "$$$$");
    }

    #[test]
    fn smartest_is_by_score_highest_first() {
        let choices = market();
        let ranking = rank(&choices).unwrap();
        assert_eq!(
            ids(&choices, &ranking.smartest)[..6],
            [
                "big/genius",
                "big/solid",
                "mid/fast",
                "mid/value",
                "mid/new",
                "low/14"
            ]
        );
        assert_eq!(ranking.smartest.len(), 20);
    }

    #[test]
    fn cheapest_and_context_only_consider_the_strongest_quarter() {
        let choices = market();
        let ranking = rank(&choices).unwrap();
        assert_eq!(
            ids(&choices, &ranking.cheapest),
            [
                "mid/value",
                "mid/fast",
                "mid/new",
                "big/solid",
                "big/genius"
            ],
            "a rock-bottom price on a weak model is not a good deal"
        );
        assert_eq!(
            ids(&choices, &ranking.context),
            [
                "big/genius",
                "big/solid",
                "mid/new",
                "mid/value",
                "mid/fast"
            ],
            "largest first, and ties go to the higher score"
        );
    }

    #[test]
    fn each_criterion_names_its_own_winner_and_the_top_five_say_why() {
        let mut choices = market();
        // The provider's number one for programming is a weaker model.
        let popular = index_of(&choices, "low/07");
        choices[popular].popular_rank = Some(0);
        let top = rank(&choices).unwrap().top;
        let listed: Vec<(&str, Vec<&str>)> = top
            .iter()
            .map(|(i, why)| (choices[*i].id.as_str(), why.clone()))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("big/genius", vec!["Smartest"]),
                ("mid/value", vec!["Best value"]),
                ("mid/fast", vec!["Smart & affordable"]),
                ("low/07", vec!["Popular for coding"]),
                ("mid/new", vec!["Newest strong"]),
            ]
        );
    }

    #[test]
    fn a_model_that_wins_two_criteria_shows_both_reasons_and_frees_a_slot() {
        let mut choices = market();
        let genius = index_of(&choices, "big/genius");
        choices[genius].popular_rank = Some(0);
        let top = rank(&choices).unwrap().top;
        assert_eq!(choices[top[0].0].id, "big/genius");
        assert_eq!(top[0].1, vec!["Smartest", "Popular for coding"]);
        assert_eq!(top.len(), TOP_COUNT, "the freed slot is filled");
        let (filler, why) = top.last().unwrap();
        assert_eq!(choices[*filler].id, "big/solid", "the next smartest model");
        assert_eq!(why, &vec!["Strong all-round"]);
        let mut distinct: Vec<usize> = top.iter().map(|(i, _)| *i).collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), top.len(), "no model is listed twice");
    }

    #[test]
    fn free_batch_and_alias_variants_never_make_the_lists() {
        let mut choices = market();
        let mut free = model("x/best:free", 99.0, (0.0, 0.0), 1_000_000, 900);
        free.popular_rank = Some(0);
        let mut batch = model("x/best:batch", 98.0, (0.5, 2.0), 1_000_000, 900);
        batch.popular_rank = Some(1);
        let alias = model("~x/latest", 97.0, (0.5, 2.0), 1_000_000, 900);
        choices.extend([free, batch, alias]);
        let ranking = rank(&choices).unwrap();
        for list in [
            &ranking.smartest,
            &ranking.cheapest,
            &ranking.context,
            &ranking.popular,
        ] {
            for id in ids(&choices, list) {
                assert!(
                    !id.contains(":free") && !id.contains(":batch") && !id.starts_with('~'),
                    "{id}"
                );
            }
        }
        assert_eq!(choices[ranking.smartest[0]].id, "big/genius");
    }

    #[test]
    fn models_that_cannot_run_an_agent_are_not_ranked() {
        let mut choices = market();
        let mut no_tools = model("x/no-tools", 99.0, (1.0, 2.0), 1_000_000, 900);
        no_tools.supports_tools = Some(false);
        let mut image_only = model("x/images", 98.0, (1.0, 2.0), 1_000_000, 900);
        image_only.text_output = Some(false);
        let tiny_context = model("x/tiny", 97.0, (1.0, 2.0), 8_000, 900);
        let mut unknown_tools = model("x/unknown", 96.0, (1.0, 2.0), 1_000_000, 900);
        unknown_tools.supports_tools = None;
        choices.extend([no_tools, image_only, tiny_context, unknown_tools]);
        let ranking = rank(&choices).unwrap();
        assert_eq!(choices[ranking.smartest[0]].id, "big/genius");
        assert_eq!(ranking.smartest.len(), 20, "only the real candidates");
    }

    #[test]
    fn popular_keeps_the_providers_order_and_needs_no_score() {
        let choices = vec![
            ModelChoice {
                id: "c/third".into(),
                popular_rank: Some(2),
                ..ModelChoice::default()
            },
            ModelChoice {
                id: "a/first".into(),
                popular_rank: Some(0),
                ..ModelChoice::default()
            },
            ModelChoice {
                id: "b/second".into(),
                popular_rank: Some(1),
                ..ModelChoice::default()
            },
            ModelChoice {
                id: "z/unranked".into(),
                ..ModelChoice::default()
            },
        ];
        let ranking = rank(&choices).unwrap();
        assert_eq!(
            ids(&choices, &ranking.popular),
            ["a/first", "b/second", "c/third"]
        );
        assert!(ranking.smartest.is_empty(), "nothing is scored");
        assert!(ranking.top.is_empty(), "five starting points need scores");
    }

    #[test]
    fn with_too_few_scores_or_none_at_all_there_is_no_ranking() {
        assert_eq!(rank(&[]), None);
        let plain = vec![
            ModelChoice {
                id: "llama3".into(),
                ..ModelChoice::default()
            };
            20
        ];
        assert_eq!(rank(&plain), None, "an id-only list has nothing to rank by");
        let few: Vec<ModelChoice> = market().into_iter().take(MIN_SCORED - 1).collect();
        assert_eq!(
            rank(&few),
            None,
            "seven scores are too few for a strongest quarter"
        );
    }
}
