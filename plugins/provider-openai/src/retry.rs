//! Exponential backoff with jitter, honouring `retry-after`.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::time::Duration;

use ch_kernel::ModelError;
use ch_kernel::reexports::CancellationToken;
use reqwest::header::HeaderMap;

/// Longest delay between attempts, whatever the server asks for.
pub const MAX_DELAY: Duration = Duration::from_secs(60);

/// Delay before retry number `attempt` (0-based): `base * 2^attempt`, capped at
/// [`MAX_DELAY`], with jitter in `[50%, 100%]`.
pub fn backoff(base: Duration, attempt: u32) -> Duration {
    let exp = base.saturating_mul(2u32.saturating_pow(attempt.min(16)));
    let capped = exp.min(MAX_DELAY);
    let random = RandomState::new().hash_one(attempt);
    let factor = 0.5 + (random % 1000) as f64 / 2000.0;
    capped.mul_f64(factor)
}

/// Parses `retry-after-ms` or `retry-after` (seconds; HTTP dates are ignored).
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let get = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok();
    let d = if let Some(ms) = get("retry-after-ms") {
        Duration::from_secs_f64(ms.max(0.0) / 1000.0)
    } else {
        Duration::from_secs_f64(get("retry-after")?.max(0.0))
    };
    Some(d.min(MAX_DELAY))
}

/// Sleeps for `delay` unless `cancel` fires first.
pub async fn sleep(delay: Duration, cancel: &CancellationToken) -> Result<(), ModelError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(ModelError::Cancelled),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn backoff_grows_and_caps() {
        let base = Duration::from_millis(1000);
        for attempt in 0..20 {
            let d = backoff(base, attempt);
            let full = base
                .saturating_mul(2u32.saturating_pow(attempt.min(16)))
                .min(MAX_DELAY);
            assert!(d >= full / 2 && d <= full, "{attempt}: {d:?}");
        }
    }

    #[test]
    fn parses_retry_after() {
        let mut h = HeaderMap::new();
        assert_eq!(retry_after(&h), None);
        h.insert("retry-after", HeaderValue::from_static("3"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(3)));
        h.insert("retry-after-ms", HeaderValue::from_static("250"));
        assert_eq!(retry_after(&h), Some(Duration::from_millis(250)));
        h.remove("retry-after-ms");
        h.insert("retry-after", HeaderValue::from_static("100000"));
        assert_eq!(retry_after(&h), Some(MAX_DELAY));
    }
}
