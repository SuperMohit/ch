//! The statistics in the report, as small pure functions.

/// z for a two-sided 95% interval.
const Z95: f64 = 1.959_963_984_540_054;

/// The 95% Wilson score interval for `passes` out of `runs`, as `(low, high)`. `None`
/// for no runs.
pub fn wilson(passes: u64, runs: u64) -> Option<(f64, f64)> {
    if runs == 0 {
        return None;
    }
    let n = runs as f64;
    let p = passes.min(runs) as f64 / n;
    let z2 = Z95 * Z95;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let half = Z95 * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / denom;
    Some(((center - half).max(0.0), (center + half).min(1.0)))
}

/// The mean, or `None` for no values.
pub fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

/// The median (the mean of the middle two for an even count), or `None` for no values.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    })
}

/// Which of two cells passed a task more often.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paired {
    A,
    B,
    Tie,
}

/// Compares two pass counts by pass rate, so cells with different numbers of runs of
/// the task compare fairly.
pub fn compare(a_passes: u64, a_runs: u64, b_passes: u64, b_runs: u64) -> Paired {
    // a_passes / a_runs vs b_passes / b_runs, without division.
    let a = u128::from(a_passes) * u128::from(b_runs);
    let b = u128::from(b_passes) * u128::from(a_runs);
    match a.cmp(&b) {
        std::cmp::Ordering::Greater => Paired::A,
        std::cmp::Ordering::Less => Paired::B,
        std::cmp::Ordering::Equal => Paired::Tie,
    }
}

/// The exact two-sided sign test: the chance, if neither cell were better, of a split
/// at least as uneven as `wins` to `losses`. Ties are left out before calling. 1 when
/// there is nothing to compare.
pub fn sign_test(wins: u64, losses: u64) -> f64 {
    let n = wins + losses;
    if n == 0 {
        return 1.0;
    }
    let k = wins.min(losses);
    // 2 * P(X <= k) for X ~ Binomial(n, 1/2), summed in log space so large n can't
    // underflow 0.5^n.
    let ln_half_n = n as f64 * 0.5f64.ln();
    let mut ln_choose = 0.0; // ln C(n, 0)
    let mut tail = 0.0;
    for i in 0..=k {
        if i > 0 {
            ln_choose += ((n - i + 1) as f64).ln() - (i as f64).ln();
        }
        tail += (ln_choose + ln_half_n).exp();
    }
    (2.0 * tail).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn wilson_matches_known_values() {
        // Reference values from the closed form.
        let (lo, hi) = wilson(8, 10).unwrap();
        assert!(close(lo, 0.4902) && close(hi, 0.9433), "{lo} {hi}");
        let (lo, hi) = wilson(0, 10).unwrap();
        assert!(close(lo, 0.0) && close(hi, 0.2775), "{lo} {hi}");
        let (lo, hi) = wilson(10, 10).unwrap();
        assert!(close(lo, 0.7225) && close(hi, 1.0), "{lo} {hi}");
        let (lo, hi) = wilson(1, 2).unwrap();
        assert!(close(lo, 0.0945) && close(hi, 0.9055), "{lo} {hi}");
        assert_eq!(wilson(0, 0), None);
    }

    #[test]
    fn mean_and_median() {
        assert_eq!(mean(&[]), None);
        assert_eq!(median(&[]), None);
        assert_eq!(mean(&[1.0, 2.0, 6.0]), Some(3.0));
        assert_eq!(median(&[6.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[5.0]), Some(5.0));
    }

    #[test]
    fn compares_by_pass_rate() {
        assert_eq!(compare(2, 3, 1, 3), Paired::A);
        assert_eq!(compare(1, 2, 2, 4), Paired::Tie);
        assert_eq!(compare(1, 3, 1, 2), Paired::B);
        assert_eq!(compare(0, 1, 0, 5), Paired::Tie);
    }

    #[test]
    fn sign_test_is_exact_and_two_sided() {
        assert_eq!(sign_test(0, 0), 1.0);
        // One differing task can't show anything.
        assert_eq!(sign_test(1, 0), 1.0);
        // 2 * (1/2)^5 = 0.0625
        assert!(close(sign_test(5, 0), 0.0625));
        assert!(close(sign_test(0, 5), 0.0625));
        // 2 * (1 + 11) / 2048
        assert!(close(sign_test(10, 1), 24.0 / 2048.0));
        // 2 * (1 + 10 + 45) / 1024 = 0.109375
        assert!(close(sign_test(8, 2), 0.109_375));
        assert_eq!(sign_test(5, 5), 1.0);
        // Large n stays finite and symmetric.
        let p = sign_test(1200, 800);
        assert!(p > 0.0 && p < 1e-15, "{p}");
        assert_eq!(sign_test(3, 9), sign_test(9, 3));
    }
}
