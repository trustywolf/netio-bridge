//! Append-only log file. Nothing goes to stdout or stderr: in the IOL image
//! they are the router console.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Repeats of one error key inside this window are counted, not written.
const ERROR_INTERVAL: Duration = Duration::from_secs(10);

struct Logger {
    file: Option<File>,
    quiet: bool,
    /// Per error key: when it was last written and how many were held back since.
    errors: HashMap<String, (Instant, u64)>,
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

pub fn init(path: &Path, quiet: bool) {
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    let _ = LOGGER.set(Mutex::new(Logger {
        file,
        quiet,
        errors: HashMap::new(),
    }));
}

fn write(l: &mut Logger, level: &str, msg: &str) {
    if let Some(f) = l.file.as_mut() {
        let _ = writeln!(f, "{} {level} {msg}", timestamp(SystemTime::now()));
    }
}

/// Start-up and lifecycle messages; dropped with `-q`.
pub fn info(msg: &str) {
    if let Some(m) = LOGGER.get() {
        let mut l = m.lock().unwrap();
        if !l.quiet {
            write(&mut l, "INFO", msg);
        }
    }
}

/// Always written: events that change what the bridge can forward.
pub fn notice(msg: &str) {
    if let Some(m) = LOGGER.get() {
        write(&mut m.lock().unwrap(), "NOTICE", msg);
    }
}

/// Errors, at most one per `key` every ten seconds.
pub fn error(key: &str, msg: &str) {
    let Some(m) = LOGGER.get() else { return };
    let mut l = m.lock().unwrap();
    let now = Instant::now();
    let held = match l.errors.get_mut(key) {
        Some((last, held)) if now.duration_since(*last) < ERROR_INTERVAL => {
            *held += 1;
            return;
        }
        Some((last, held)) => {
            *last = now;
            std::mem::take(held)
        }
        None => {
            l.errors.insert(key.to_string(), (now, 0));
            0
        }
    };
    if held > 0 {
        write(
            &mut l,
            "ERROR",
            &format!("{msg} ({held} more since the last report)"),
        );
    } else {
        write(&mut l, "ERROR", msg);
    }
}

/// UTC time as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
fn timestamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (y, mo, da) = civil_from_days((secs / 86_400) as i64);
    let s = secs % 86_400;
    format!(
        "{y:04}-{mo:02}-{da:02}T{:02}:{:02}:{:02}.{:03}Z",
        s / 3600,
        s / 60 % 60,
        s % 60,
        d.subsec_millis()
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// public-domain algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_757), (2026, 10, 31));
    }

    #[test]
    fn timestamp_format() {
        let t = UNIX_EPOCH + Duration::from_millis(1_793_491_200_123);
        assert_eq!(timestamp(t), "2026-11-01T00:00:00.123Z");
    }
}
