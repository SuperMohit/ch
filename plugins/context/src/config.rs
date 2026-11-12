//! `[plugins.context]`: the strategy and its limits.

use anyhow::bail;
use serde::Deserialize;

/// How the context is managed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// The whole history is sent; an overflow ends the turn.
    None,
    /// Old, bulky content is trimmed.
    Elide,
    /// Summaries only. Not available yet.
    Summarize,
    /// Trims first, then summaries. Not available yet.
    Staged,
}

/// `[plugins.context]`. Shares are of the usable window: the context window minus the
/// reserved output, the system prompt and the tools.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub strategy: Strategy,
    /// Above this share, the middle is trimmed.
    pub soft: f64,
    /// Above this share, a summary is written (from `summarize` and `staged`).
    pub hard: f64,
    /// The recent window kept word for word: at least this share...
    pub recent: f64,
    /// ...and at least this many steps.
    pub min_recent_steps: usize,
    /// A trim goes down to this share...
    pub trim_to: f64,
    /// ...and is made only if it frees at least this share.
    pub min_free: f64,
    /// Longer tool results are trimmed.
    pub elide_min_chars: usize,
    /// Longer strings in a tool call's input are trimmed.
    pub input_min_chars: usize,
    /// Tools whose every call but the latest is trimmed first.
    pub superseded_tools: Vec<String>,
    /// The longest summary (from `summarize` and `staged`).
    pub summary_max_words: usize,
    /// Above this share of summaries, the two oldest are merged (from `summarize` and
    /// `staged`).
    pub max_summary_share: f64,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            strategy: Strategy::Elide,
            soft: 0.60,
            hard: 0.85,
            recent: 0.30,
            min_recent_steps: 2,
            trim_to: 0.40,
            min_free: 0.10,
            elide_min_chars: 500,
            input_min_chars: 1000,
            superseded_tools: vec!["update_plan".into()],
            summary_max_words: 1000,
            max_summary_share: 0.15,
        }
    }
}

impl ContextConfig {
    /// Every key, all safe to set in a project without trust: none runs a command, reads
    /// a path or picks an endpoint.
    pub const KEYS: &[&str] = &[
        "strategy",
        "soft",
        "hard",
        "recent",
        "min_recent_steps",
        "trim_to",
        "min_free",
        "elide_min_chars",
        "input_min_chars",
        "superseded_tools",
        "summary_max_words",
        "max_summary_share",
    ];

    /// Rejects a strategy that isn't available yet, and moves each value that is out of
    /// range to the nearest allowed one. Returns a warning for each value moved.
    pub fn checked(mut self) -> anyhow::Result<(Self, Vec<String>)> {
        if let s @ (Strategy::Summarize | Strategy::Staged) = self.strategy {
            let name = if s == Strategy::Summarize {
                "summarize"
            } else {
                "staged"
            };
            bail!(
                "plugins.context.strategy = \"{name}\" isn't available yet; use \"elide\" or \
                 \"none\""
            );
        }
        let mut warnings = Vec::new();
        let mut share = |name: &str, value: &mut f64, lo: f64, hi: f64| {
            let lo = lo.min(hi);
            let fixed = if value.is_nan() {
                lo
            } else {
                value.clamp(lo, hi)
            };
            if fixed != *value {
                warnings.push(format!(
                    "plugins.context.{name} = {value} is outside {lo:.2}–{hi:.2}; using {fixed:.2}"
                ));
                *value = fixed;
            }
        };
        share("soft", &mut self.soft, 0.30, 0.90);
        share("hard", &mut self.hard, (self.soft + 0.05).max(0.50), 0.95);
        share(
            "recent",
            &mut self.recent,
            0.10,
            (self.soft - 0.05).min(0.50),
        );
        share("trim_to", &mut self.trim_to, 0.20, self.soft - 0.05);
        share("min_free", &mut self.min_free, 0.02, 0.30);
        share("max_summary_share", &mut self.max_summary_share, 0.05, 0.30);
        let mut count = |name: &str, value: &mut usize, lo: usize, hi: usize| {
            let fixed = (*value).clamp(lo, hi);
            if fixed != *value {
                warnings.push(format!(
                    "plugins.context.{name} = {value} is outside {lo}–{hi}; using {fixed}"
                ));
                *value = fixed;
            }
        };
        count("min_recent_steps", &mut self.min_recent_steps, 1, 10);
        count("elide_min_chars", &mut self.elide_min_chars, 200, 20_000);
        count("input_min_chars", &mut self.input_min_chars, 200, 20_000);
        count("summary_max_words", &mut self.summary_max_words, 200, 3000);
        Ok((self, warnings))
    }
}

#[cfg(test)]
mod tests {
    use ch_kernel::Config;

    use super::*;

    fn parse(toml: &str) -> anyhow::Result<ContextConfig> {
        Config::parse(toml)?.plugin_section("context")
    }

    #[test]
    fn the_defaults_are_the_documented_ones_and_need_no_correction() {
        let (config, warnings) = parse("").unwrap().checked().unwrap();
        assert_eq!(config, ContextConfig::default());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.strategy, Strategy::Elide);
    }

    #[test]
    fn out_of_range_values_are_moved_with_a_warning() {
        let config = parse(
            "[plugins.context]\nsoft = 0.95\nhard = 0.5\nrecent = 0.9\ntrim_to = 0.1\n\
             min_free = 1.0\nmin_recent_steps = 0\nelide_min_chars = 5\ninput_min_chars = 1000000",
        )
        .unwrap();
        let (config, warnings) = config.checked().unwrap();
        assert_eq!(config.soft, 0.90);
        assert!((config.hard - 0.95).abs() < 1e-9, "{}", config.hard);
        assert_eq!(config.recent, 0.50);
        assert_eq!(config.trim_to, 0.20);
        assert_eq!(config.min_free, 0.30);
        assert_eq!(config.min_recent_steps, 1);
        assert_eq!(config.elide_min_chars, 200);
        assert_eq!(config.input_min_chars, 20_000);
        assert_eq!(warnings.len(), 8, "{warnings:#?}");
        assert!(
            warnings[0].contains("plugins.context.soft = 0.95 is outside 0.30–0.90; using 0.90"),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn limits_that_depend_on_soft_follow_it() {
        let (config, _) = parse("[plugins.context]\nsoft = 0.4\nrecent = 0.38\ntrim_to = 0.39")
            .unwrap()
            .checked()
            .unwrap();
        assert!((config.recent - 0.35).abs() < 1e-9, "{}", config.recent);
        assert!((config.trim_to - 0.35).abs() < 1e-9, "{}", config.trim_to);
    }

    #[test]
    fn a_wrong_type_or_an_unknown_key_is_a_config_error() {
        assert!(parse("[plugins.context]\nsoft = \"high\"").is_err());
        assert!(parse("[plugins.context]\nmin_recent_steps = -1").is_err());
        assert!(parse("[plugins.context]\nsof = 0.5").is_err());
        assert!(parse("[plugins.context]\nstrategy = \"magic\"").is_err());
    }

    #[test]
    fn strategies_that_need_summaries_are_refused_for_now() {
        for name in ["summarize", "staged"] {
            let err = parse(&format!("[plugins.context]\nstrategy = \"{name}\""))
                .unwrap()
                .checked()
                .unwrap_err();
            assert!(err.to_string().contains("isn't available yet"), "{err}");
        }
        let (config, _) = parse("[plugins.context]\nstrategy = \"none\"")
            .unwrap()
            .checked()
            .unwrap();
        assert_eq!(config.strategy, Strategy::None);
    }
}
