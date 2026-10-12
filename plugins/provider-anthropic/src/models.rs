//! What each Claude model accepts: thinking style, sampling parameters, context window.
//!
//! The rules follow the current Messages API: models from the 4.6 generation on use
//! adaptive thinking (`budget_tokens` is rejected from 4.7 on), and the newest models reject
//! sampling parameters such as `temperature`.

/// How extended thinking is requested for a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingStyle {
    /// `{"type": "adaptive"}`; depth is controlled with `output_config.effort`.
    Adaptive,
    /// `{"type": "enabled", "budget_tokens": N}` (older models).
    Budget,
}

/// Request-shape capabilities of one model id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCaps {
    pub thinking: ThinkingStyle,
    /// Whether `temperature` may be sent at all.
    pub sampling: bool,
    /// Whether `thinking.display` is accepted (and needed: these models hide thinking text
    /// unless `display: "summarized"` is sent).
    pub thinking_display: bool,
    /// Thinking runs even when the `thinking` parameter is omitted.
    pub always_thinks: bool,
    /// Supports the server-side refusal fallback (`fallbacks: "default"`).
    pub refusal_fallback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    Mythos,
}

/// Parses ids like `claude-opus-5-5`, `claude-haiku-4-5-20251001`,
/// `us.anthropic.claude-sonnet-4-6` or `claude-3-5-sonnet-20241022` into a family and a
/// `(major, minor)` version. Claude 3 era ids (version before the family) return `(3, x)`.
fn parse(model: &str) -> Option<(Family, (u32, u32))> {
    let lower = model.to_ascii_lowercase();
    let start = lower.find("claude-")?;
    let parts: Vec<&str> = lower[start + "claude-".len()..]
        .split(['-', '@', ':'])
        .collect();
    let family_at = parts
        .iter()
        .position(|p| matches!(*p, "opus" | "sonnet" | "haiku" | "fable" | "mythos"))?;
    let family = match parts[family_at] {
        "opus" => Family::Opus,
        "sonnet" => Family::Sonnet,
        "haiku" => Family::Haiku,
        "fable" => Family::Fable,
        _ => Family::Mythos,
    };
    // Version parts are 1-2 digit numbers; date suffixes (8 digits) are not.
    let num = |i: usize| -> Option<u32> {
        let s = parts.get(i)?;
        if s.len() <= 2 { s.parse().ok() } else { None }
    };
    // Old style ids put the version first: claude-3-5-sonnet-...
    let first = if family_at > 0 { 0 } else { 1 };
    let version = (num(first)?, num(first + 1).unwrap_or(0));
    Some((family, version))
}

/// Capabilities for `model`. Unknown ids get the conservative legacy shape.
pub fn caps(model: &str) -> ModelCaps {
    let legacy = ModelCaps {
        thinking: ThinkingStyle::Budget,
        sampling: true,
        thinking_display: false,
        always_thinks: false,
        refusal_fallback: false,
    };
    let modern = ModelCaps {
        thinking: ThinkingStyle::Adaptive,
        sampling: false,
        thinking_display: true,
        always_thinks: true,
        refusal_fallback: false,
    };
    let Some((family, v)) = parse(model) else {
        return legacy;
    };
    let fallback = matches!(
        (family, v),
        (Family::Opus, (5, 0))
            | (Family::Opus, (5, 5))
            | (Family::Sonnet, (5, 5))
            | (Family::Fable, (5, 1))
    );
    match family {
        Family::Fable | Family::Mythos => ModelCaps {
            refusal_fallback: fallback,
            ..modern
        },
        Family::Opus | Family::Sonnet | Family::Haiku if v.0 >= 5 => ModelCaps {
            refusal_fallback: fallback,
            ..modern
        },
        Family::Opus if v >= (4, 7) => ModelCaps {
            always_thinks: false,
            ..modern
        },
        Family::Opus | Family::Sonnet if v == (4, 6) => ModelCaps {
            thinking: ThinkingStyle::Adaptive,
            ..legacy
        },
        _ => legacy,
    }
}

/// Known context windows (input tokens). `None` for ids this table doesn't know.
pub fn context_window(model: &str) -> Option<u64> {
    let (family, v) = parse(model)?;
    const M: u64 = 1_000_000;
    const K200: u64 = 200_000;
    Some(match family {
        Family::Fable | Family::Mythos => M,
        Family::Opus | Family::Sonnet if v >= (4, 6) => M,
        Family::Haiku if v.0 >= 5 => M,
        // Claude 3.x and 4.0-4.5 models, including Haiku 4.5.
        _ if v.0 >= 3 => K200,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ids() {
        assert_eq!(parse("claude-opus-5-5"), Some((Family::Opus, (5, 5))));
        assert_eq!(parse("claude-opus-5"), Some((Family::Opus, (5, 0))));
        assert_eq!(
            parse("claude-haiku-4-5-20251001"),
            Some((Family::Haiku, (4, 5)))
        );
        assert_eq!(
            parse("us.anthropic.claude-sonnet-4-6"),
            Some((Family::Sonnet, (4, 6)))
        );
        assert_eq!(
            parse("claude-3-5-sonnet-20241022"),
            Some((Family::Sonnet, (3, 5)))
        );
        assert_eq!(
            parse("claude-opus-4-5@20251101"),
            Some((Family::Opus, (4, 5)))
        );
        assert_eq!(parse("gpt-4o"), None);
    }

    #[test]
    fn capabilities_by_generation() {
        let opus55 = caps("claude-opus-5-5");
        assert_eq!(opus55.thinking, ThinkingStyle::Adaptive);
        assert!(!opus55.sampling && opus55.always_thinks && opus55.refusal_fallback);

        let opus48 = caps("claude-opus-4-8");
        assert_eq!(opus48.thinking, ThinkingStyle::Adaptive);
        assert!(!opus48.sampling && !opus48.always_thinks && !opus48.refusal_fallback);

        let sonnet46 = caps("claude-sonnet-4-6");
        assert_eq!(sonnet46.thinking, ThinkingStyle::Adaptive);
        assert!(sonnet46.sampling && !sonnet46.thinking_display);

        let haiku45 = caps("claude-haiku-4-5");
        assert_eq!(haiku45.thinking, ThinkingStyle::Budget);
        assert!(haiku45.sampling);

        assert!(caps("claude-haiku-5-5").always_thinks);
        assert!(!caps("claude-haiku-5-5").refusal_fallback);
        assert!(caps("claude-fable-5-1").refusal_fallback);
        assert!(!caps("claude-fable-5").refusal_fallback);
        assert_eq!(caps("my-proxy-model").thinking, ThinkingStyle::Budget);
    }

    #[test]
    fn windows() {
        assert_eq!(context_window("claude-opus-5-5"), Some(1_000_000));
        assert_eq!(context_window("claude-sonnet-5-5"), Some(1_000_000));
        assert_eq!(context_window("claude-haiku-5-5"), Some(1_000_000));
        assert_eq!(context_window("claude-fable-5-1"), Some(1_000_000));
        assert_eq!(context_window("claude-opus-4-6"), Some(1_000_000));
        assert_eq!(context_window("claude-haiku-4-5"), Some(200_000));
        assert_eq!(context_window("claude-sonnet-4-5"), Some(200_000));
        assert_eq!(context_window("something-else"), None);
    }
}
