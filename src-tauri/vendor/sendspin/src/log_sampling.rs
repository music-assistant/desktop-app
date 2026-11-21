// ABOUTME: Shared predicates for sampling high-frequency log lines
// ABOUTME: Keeps trace/debug logs useful without flooding normal runs

/// Return `true` for counts 1-5, 10, and every multiple of 100.
///
/// Intended for high-frequency diagnostic logs where startup behavior matters
/// but every steady-state event is redundant: callers see every early event,
/// one checkpoint at 10, then a periodic heartbeat. `count` is one-based; zero
/// returns `false`.
pub(crate) fn should_log_sample(count: u64) -> bool {
    count != 0 && (count <= 5 || count == 10 || count.is_multiple_of(100))
}

#[cfg(test)]
mod tests {
    use super::should_log_sample;

    #[test]
    fn samples_first_few_ten_and_every_hundred() {
        let sampled: Vec<u64> = (0..=205)
            .filter(|&count| should_log_sample(count))
            .collect();
        assert_eq!(sampled, vec![1, 2, 3, 4, 5, 10, 100, 200]);
    }
}
