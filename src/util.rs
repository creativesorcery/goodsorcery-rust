//! Small helpers shared by every part of the client: text caps, timestamps,
//! random numbers, UUIDv7 ids, locks that never panic and throttled logging.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Locks a mutex, recovering the data if another thread panicked while
/// holding it. The client never panics into the host app because of a
/// poisoned lock.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The first `max` characters of `text` (never splits a character).
pub(crate) fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => text[..at].to_string(),
        None => text.to_string(),
    }
}

/// Like [`truncate`], but in place.
pub(crate) fn truncate_in_place(text: &mut String, max: usize) {
    if let Some((at, _)) = text.char_indices().nth(max) {
        text.truncate(at);
    }
}

/// Seconds since the Unix epoch, as a float (0 before the epoch).
pub(crate) fn unix_seconds(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Milliseconds since the Unix epoch (0 before the epoch).
pub(crate) fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)).unwrap_or(0)
}

/// The start of the UTC minute holding `time`, in seconds since the epoch.
pub(crate) fn minute_of(time: SystemTime) -> u64 {
    let secs = time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    secs - secs % 60
}

/// `2026-11-08T10:02:44.518Z`
pub(crate) fn iso8601_millis(time: SystemTime) -> String {
    let millis = unix_millis(time);
    let (date, clock) = civil(millis / 1000);
    format!("{date}T{clock}.{:03}Z", millis % 1000)
}

/// `2026-11-04T14:32:00Z` for a whole number of seconds.
pub(crate) fn iso8601_seconds(secs: u64) -> String {
    let (date, clock) = civil(secs);
    format!("{date}T{clock}Z")
}

/// Splits Unix seconds into `YYYY-MM-DD` and `HH:MM:SS` (proleptic
/// Gregorian, Howard Hinnant's days-to-civil algorithm).
fn civil(secs: u64) -> (String, String) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (format!("{year:04}-{month:02}-{day:02}"), format!("{:02}:{:02}:{:02}", rem / 3_600, (rem / 60) % 60, rem % 60))
}

/// A random 64-bit number. Seeded per call from the standard library's
/// randomly keyed SipHash, a counter and the clock: unpredictable enough for
/// ids and jitter, without a dependency on an RNG crate. Not for secrets.
pub(crate) fn random_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    hasher.finish()
}

/// A random float in `[0, 1)`.
pub(crate) fn random_unit() -> f64 {
    (random_u64() >> 11) as f64 / (1u64 << 53) as f64
}

/// A UUIDv7: the first 48 bits are `time` in Unix milliseconds, the rest
/// random apart from the version and variant bits.
pub(crate) fn uuid_v7(time: SystemTime) -> [u8; 16] {
    let millis = unix_millis(time);
    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6..14].copy_from_slice(&random_u64().to_be_bytes());
    bytes[14..].copy_from_slice(&random_u64().to_be_bytes()[..2]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

/// The hyphenated lowercase form of a UUID.
pub(crate) fn format_uuid(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A fresh UUIDv7 string for now.
pub(crate) fn new_uuid() -> String {
    format_uuid(&uuid_v7(SystemTime::now()))
}

/// Rounds to three decimals, mapping NaN and infinities to 0 (JSON has no
/// such numbers).
pub(crate) fn round3(value: f64) -> f64 {
    if value.is_finite() { (value * 1000.0).round() / 1000.0 } else { 0.0 }
}

/// Milliseconds in a duration, as a float.
pub(crate) fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Shows an API key as `gs_test_…e81a`, never in full.
pub(crate) fn masked_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() < 12 {
        return "…".to_string();
    }
    let prefix: String =
        match key.find('_').and_then(|first| key[first + 1..].find('_').map(|second| first + second + 2)) {
            Some(end) if end <= 8 => key[..end].to_string(),
            _ => String::new(),
        };
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}…{suffix}")
}

/// The client's own log lines, on stderr with a `[goodsorcery]` prefix.
/// They name classes, counts and statuses only, never captured data.
/// Warnings can be throttled per key; debug lines show only with
/// `Config::debug(true)`.
pub(crate) mod log {
    use super::*;

    static DEBUG: AtomicBool = AtomicBool::new(false);
    static LAST: OnceLock<Mutex<HashMap<&'static str, Option<Instant>>>> = OnceLock::new();

    pub(crate) fn set_debug(on: bool) {
        DEBUG.store(on, Ordering::Relaxed);
    }

    pub(crate) fn debug(message: impl AsRef<str>) {
        if DEBUG.load(Ordering::Relaxed) {
            write_line(message.as_ref());
        }
    }

    pub(crate) fn warn(message: impl AsRef<str>) {
        write_line(message.as_ref());
    }

    /// Writes one line to stderr, ignoring failures: `eprintln!` panics
    /// when stderr is a broken pipe, and these lines are written from app
    /// threads and from the panic hook.
    fn write_line(message: &str) {
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "[goodsorcery] {message}");
    }

    /// Logs at most once per process for `key`.
    pub(crate) fn warn_once(key: &'static str, message: impl AsRef<str>) {
        let map = LAST.get_or_init(|| Mutex::new(HashMap::new()));
        let first = lock(map).insert(key, None).is_none();
        if first {
            warn(message);
        }
    }

    /// Logs at most once every `every` for `key`.
    pub(crate) fn warn_every(key: &'static str, every: Duration, message: impl AsRef<str>) {
        let map = LAST.get_or_init(|| Mutex::new(HashMap::new()));
        let now = Instant::now();
        let due = {
            let mut map = lock(map);
            let due = match map.get(key) {
                Some(Some(last)) => now.duration_since(*last) >= every,
                Some(None) => false,
                None => true,
            };
            if due {
                map.insert(key, Some(now));
            }
            due
        };
        if due {
            warn(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_on_character_boundaries() {
        assert_eq!(truncate("héllo", 2), "hé");
        assert_eq!(truncate("abc", 10), "abc");
        let mut text = String::from("ünïcode");
        truncate_in_place(&mut text, 3);
        assert_eq!(text, "ünï");
    }

    #[test]
    fn formats_timestamps() {
        let time = UNIX_EPOCH + Duration::from_millis(1_762_596_164_518);
        assert_eq!(iso8601_millis(time), "2025-11-08T10:02:44.518Z");
        assert_eq!(iso8601_seconds(1_793_802_720), "2026-11-04T14:32:00Z");
        assert_eq!(iso8601_seconds(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(minute_of(time), 1_762_596_120);
    }

    #[test]
    fn uuid_v7_carries_the_time_version_and_variant() {
        let time = UNIX_EPOCH + Duration::from_millis(0x0192_f7a4_6c1e);
        let bytes = uuid_v7(time);
        let text = format_uuid(&bytes);
        assert!(text.starts_with("0192f7a4-6c1e-7"), "{text}");
        assert_eq!(text.len(), 36);
        assert!(matches!(text.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert_ne!(uuid_v7(time), uuid_v7(time));
    }

    #[test]
    fn random_unit_stays_in_range() {
        for _ in 0..1_000 {
            let value = random_unit();
            assert!((0.0..1.0).contains(&value));
        }
    }

    #[test]
    fn masks_api_keys() {
        assert_eq!(masked_key("gs_test_9f2c0000e81a"), "gs_test_…e81a");
        assert_eq!(masked_key("short"), "…");
        assert_eq!(masked_key("abcdefghijklmnop"), "…mnop");
    }

    #[test]
    fn rounds_and_rejects_non_finite_numbers() {
        assert_eq!(round3(1.23456), 1.235);
        assert_eq!(round3(f64::NAN), 0.0);
        assert_eq!(round3(f64::INFINITY), 0.0);
    }
}
