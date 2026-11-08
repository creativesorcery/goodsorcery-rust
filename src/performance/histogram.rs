//! The fixed log-scale duration buckets (scheme `gs-v1`), shared literally
//! with the server and the Ruby gem.

pub(crate) const SCHEME: &str = "gs-v1";

/// Upper bounds (ms) of buckets 0–29; bucket 30 holds 48,000 ms and up.
pub(crate) const BOUNDS_MS: [f64; 30] = [
    1.0, 1.5, 2.0, 3.0, 4.5, 6.5, 9.5, 14.0, 20.0, 28.0, 40.0, 60.0, 85.0, 125.0, 180.0, 260.0, 380.0, 550.0, 800.0,
    1_150.0, 1_700.0, 2_450.0, 3_550.0, 5_100.0, 7_500.0, 11_000.0, 16_000.0, 23_000.0, 33_000.0, 48_000.0,
];

/// The bucket (0–30) for a duration: the number of bounds `<=` it.
pub(crate) fn index(duration_ms: f64) -> usize {
    if !duration_ms.is_finite() || duration_ms < 0.0 {
        return 0;
    }
    BOUNDS_MS.partition_point(|bound| *bound <= duration_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_match_the_protocol() {
        assert_eq!(
            BOUNDS_MS,
            [
                1.0, 1.5, 2.0, 3.0, 4.5, 6.5, 9.5, 14.0, 20.0, 28.0, 40.0, 60.0, 85.0, 125.0, 180.0, 260.0, 380.0,
                550.0, 800.0, 1150.0, 1700.0, 2450.0, 3550.0, 5100.0, 7500.0, 11000.0, 16000.0, 23000.0, 33000.0,
                48000.0
            ]
        );
    }

    #[test]
    fn indexes() {
        assert_eq!(index(0.0), 0);
        assert_eq!(index(0.4), 0);
        assert_eq!(index(0.999), 0);
        assert_eq!(index(1.0), 1);
        assert_eq!(index(250.0), 15);
        assert_eq!(index(47_999.0), 29);
        assert_eq!(index(48_000.0), 30);
        assert_eq!(index(60_000.0), 30);
        assert_eq!(index(1e6), 30);
        assert_eq!(index(f64::NAN), 0);
        assert_eq!(index(-1.0), 0);
    }
}
