//! Schedule computation for `.timer` units.
//!
//! Implements monotonic triggers (`OnBootSec=`, `OnStartupSec=`,
//! `OnActiveSec=`, `OnUnitActiveSec=`, `OnUnitInactiveSec=`) and wall-clock
//! [`calendar::CalendarSpec`] triggers (`OnCalendar=`).
//!
//! Time is expressed as unix-epoch seconds (`u64`); calendar matches are
//! evaluated in local wall-clock time.  `OnUnitActiveSec=` /
//! `OnUnitInactiveSec=` are anchored to this worker's own trigger time
//! (System C is not the unit owner), which matches systemd closely enough
//! for self-triggering chains.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike};
use sysa::proto::TimerConfig;

/// A parsed calendar component set: (year, month, day) for dates and
/// (hour, minute, second) for times.  Empty set means "any".
type CalendarPart = BTreeSet<u32>;

/// Current unix epoch in seconds.
pub fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current local wall-clock time (naive — that is what calendar specs match).
fn local_now() -> NaiveDateTime {
    Local::now().naive_local()
}

/// Epoch of the last machine boot, used as the `OnBootSec=` anchor.
pub fn boot_epoch() -> u64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        if let Some(btime) = btime_from_proc_stat() {
            return btime;
        }
    }
    #[cfg(any(
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "macos"
    ))]
    {
        if let Some(boot) = boottime_sysctl() {
            return boot;
        }
    }
    epoch_now()
}

/// Read `btime` (boot epoch) from `/proc/stat`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn btime_from_proc_stat() -> Option<u64> {
    let data = std::fs::read_to_string("/proc/stat").ok()?;
    for line in data.lines() {
        if let Some(rest) = line.strip_prefix("btime ") {
            return rest.trim().parse::<u64>().ok();
        }
    }
    None
}

/// Read the boot time from `sysctl kern.boottime`.
#[cfg(any(
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "macos"
))]
fn boottime_sysctl() -> Option<u64> {
    unsafe {
        let mut tv: libc::timeval = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::timeval>() as libc::size_t;
        let name = b"kern.boottime\0";
        let r = libc::sysctlbyname(
            name.as_ptr() as *const libc::c_char,
            &mut tv as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        );
        if r == 0 {
            Some(tv.tv_sec as u64)
        } else {
            None
        }
    }
}

/// Convert a local wall-clock instant to a unix epoch.
/// Returns `None` for times that do not exist in the local timezone (DST
/// spring-forward gaps), which are simply skipped.
fn ndt_to_epoch(t: NaiveDateTime) -> Option<u64> {
    let dt = Local.from_local_datetime(&t).single()?;
    Some(dt.timestamp().max(0) as u64)
}

/// Tiny deterministic PRNG for `RandomizedDelaySec=` jitter.
static PRNG_STATE: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

/// Uniform value in `0..n` using xorshift64.
fn rand_below(n: u64) -> u64 {
    if n <= 1 {
        return 0;
    }
    loop {
        let mut x = PRNG_STATE.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        PRNG_STATE.store(x, Ordering::Relaxed);
        if x == 0 {
            continue;
        }
        // Rejection-free modulus for small n via multiplication is overkill
        // here; a full-resolution remainder is perfectly fine for jitter.
        return x % n;
    }
}

// ---------------------------------------------------------------------------
// Calendar specifications
// ---------------------------------------------------------------------------

/// A parsed `OnCalendar=` specification (systemd-style subset).
///
/// Supported grammar (space-separated): `[day-of-week] [date] [time]`, e.g.
/// `Mon..Fri *-*-* 09:00:00`, `*-*-* 00:00:00`, `daily`, `hourly`, `weekly`.
/// Date components: `*`, a number, or a `..` range (`*-12-24`, `2026-01-01`,
/// `1..15-*-*`).  Time components: `*`, a number, or a `..` range
/// (`09:00`, `9..17:00`, `*:*:*`).  Day-of-week: names and `Mon..Fri`
/// ranges (possibly wrapping, e.g. `Fri..Mon`).  An empty component set means
/// "any value".  A fully-wildcarded time (`*:*:*`) matches every second.
#[derive(Debug, Clone)]
pub struct CalendarSpec {
    // 1=Mon .. 7=Sun; None = any.
    weekdays: Option<BTreeSet<u32>>,
    // Empty set = any value.
    year: BTreeSet<u32>,
    month: BTreeSet<u32>,
    day: BTreeSet<u32>,
    hour: BTreeSet<u32>,
    minute: BTreeSet<u32>,
    second: BTreeSet<u32>,
}

/// Expand a parsed component set to an ascending list of concrete values.
fn expanded(min: u32, max: u32, set: &BTreeSet<u32>) -> Vec<u32> {
    if set.is_empty() {
        (min..=max).collect()
    } else {
        set.iter().copied().collect()
    }
}

/// Parse one numeric component of a date/time domain.
/// `*` (or empty) yields an empty set (any); supports `n` and `n..m`
/// (inclusive) and comma separators.
fn parse_comp(tok: &str, min: u32, max: u32) -> Result<BTreeSet<u32>, String> {
    let mut set = BTreeSet::new();
    let tok = tok.trim().to_ascii_lowercase();
    if tok.is_empty() || tok == "*" {
        return Ok(set);
    }
    for part in tok.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (lo, hi) = match part.split_once("..") {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (part, part),
        };
        let lo: u32 = lo.parse().map_err(|_| {
            sysa::l10n::fmt(
                sysa::l10n::t_("invalid value '{part}' in component"),
                &[("part", &part.to_string())],
            )
        })?;
        let hi: u32 = hi.parse().map_err(|_| {
            sysa::l10n::fmt(
                sysa::l10n::t_("invalid value '{part}' in component"),
                &[("part", &part.to_string())],
            )
        })?;
        if lo < min || hi > max || lo > hi {
            return Err(sysa::l10n::fmt(
                sysa::l10n::t_("component '{part}' out of range ({min}..{max})"),
                &[
                    ("part", &part.to_string()),
                    ("min", &min.to_string()),
                    ("max", &max.to_string()),
                ],
            ));
        }
        for v in lo..=hi {
            set.insert(v);
        }
    }
    Ok(set)
}

fn parse_weekdays(tok: &str) -> Result<Option<BTreeSet<u32>>, String> {
    let tok = tok.trim().to_ascii_lowercase();
    if tok == "*" || tok.is_empty() {
        return Ok(None);
    }
    let names = [
        "mon", "tue", "wed", "thu", "fri", "sat", "sun",
    ];
    let idx = |s: &str| -> Option<u32> {
        names.iter().position(|n| *n == s).map(|i| i as u32 + 1)
    };
    let mut set = BTreeSet::new();
    for part in tok.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (a, b) = match part.split_once("..").or_else(|| part.split_once('-')) {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (part, part),
        };
        let lo = idx(a).ok_or_else(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("invalid weekday '{a}'"),
                &[("a", &a.to_string())],
            )
        })?;
        let hi = idx(b).ok_or_else(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("invalid weekday '{b}'"),
                &[("b", &b.to_string())],
            )
        })?;
        if lo > hi {
            // Wrapping range (e.g. Fri..Mon): insert lo..=7 and 1..=hi.
            for v in lo..=7 {
                set.insert(v);
            }
            for v in 1..=hi {
                set.insert(v);
            }
            continue;
        }
        for v in lo..=hi {
            set.insert(v);
        }
    }
    Ok(Some(set))
}
fn is_weekday_tok(tok: &str) -> bool {
    let names = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let t = tok.trim().to_ascii_lowercase();
    if t == "*" {
        return false;
    }
    let mut all_names = true;
    for part in t.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (a, b) = match part.split_once("..").or_else(|| part.split_once('-')) {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (part, part),
        };
        if !names.contains(&a) || !names.contains(&b) {
            all_names = false;
            break;
        }
    }
    all_names
}

fn is_time_tok(tok: &str) -> bool {
    tok.contains(':')
}

fn is_date_tok(tok: &str) -> bool {
    tok.contains('-')
}

impl CalendarSpec {
    /// Parse an `OnCalendar=` value into a spec.
    pub fn parse(input: &str) -> Result<CalendarSpec, String> {
        let s = input.trim();
        if s.is_empty() {
            return Err(sysa::l10n::t_("empty calendar spec").to_string());
        }
        if let Some(spec) = parse_shorthand(s) {
            return Ok(spec);
        }

        let tokens: Vec<&str> = s.split_whitespace().collect();
        let (weekdays, date_tok, time_tok) = match tokens.len() {
            1 => {
                let tok = tokens[0];
                if is_time_tok(tok) {
                    (None, None, Some(tok))
                } else if is_weekday_tok(tok) {
                    (parse_weekdays(tok)?, None, None)
                } else if is_date_tok(tok) {
                    (None, Some(tok), None)
                } else {
                    return Err(sysa::l10n::fmt(
                        sysa::l10n::t_("unrecognised timer token '{tok}'"),
                        &[("tok", &tok.to_string())],
                    ));
                }
            }
            2 => {
                let (t0, t1) = (tokens[0], tokens[1]);
                if is_weekday_tok(t0) {
                    (parse_weekdays(t0)?, None, Some(t1))
                } else {
                    (None, Some(t0), Some(t1))
                }
            }
            3 => (parse_weekdays(tokens[0])?, Some(tokens[1]), Some(tokens[2])),
            _ => {
                return Err(sysa::l10n::fmt(
                    sysa::l10n::t_("too many fields in OnCalendar spec '{s}'"),
                    &[("s", &s.to_string())],
                ))
            }
        };

        let (year, month, day) = match date_tok {
            None => (BTreeSet::new(), BTreeSet::new(), BTreeSet::new()),
            Some(t) => parse_date(t)?,
        };

        let (hour, minute, second) = match time_tok {
            None => (BTreeSet::new(), BTreeSet::from([0]), BTreeSet::from([0])),
            Some(t) => parse_time(t)?,
        };

        Ok(CalendarSpec {
            weekdays,
            year,
            month,
            day,
            hour,
            minute,
            second,
        })
    }

    fn date_matches(&self, date: NaiveDate) -> bool {
        if !self.year.is_empty() && !self.year.contains(&(date.year() as u32)) {
            return false;
        }
        if !self.month.is_empty() && !self.month.contains(&date.month()) {
            return false;
        }
        if !self.day.is_empty() && !self.day.contains(&date.day()) {
            return false;
        }
        if let Some(wd) = &self.weekdays {
            if !wd.contains(&date.weekday().number_from_monday()) {
                return false;
            }
        }
        true
    }

    /// Earliest matching instant strictly after `now`.
    pub fn next_after(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        let mut day = now.date();
        for _ in 0..=(366 * 2) {
            if self.date_matches(day) {
                if let Some(t) = self.time_after(now, day) {
                    return Some(NaiveDateTime::new(day, t));
                }
            }
            day += Duration::days(1);
        }
        None
    }

    /// Latest matching instant strictly before `now`.
    pub fn prev_before(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        let mut day = now.date();
        for _ in 0..=(366 * 2) {
            if self.date_matches(day) {
                if let Some(t) = self.time_before(now, day) {
                    return Some(NaiveDateTime::new(day, t));
                }
            }
            day -= Duration::days(1);
        }
        None
    }

    /// Earliest matching instant strictly after `now`, as a unix epoch.
    /// Returns `None` for specs without a future match or for matches that
    /// fall in a DST gap.
    pub fn next_epoch(&self, now: NaiveDateTime) -> Option<u64> {
        ndt_to_epoch(self.next_after(now)?)
    }

    fn time_after(&self, now: NaiveDateTime, day: NaiveDate) -> Option<NaiveTime> {
        let same_day = day == now.date();
        let (now_h, now_m, now_s) = (now.hour(), now.minute(), now.second());
        let hours = expanded(0, 23, &self.hour);
        let minutes = expanded(0, 59, &self.minute);
        let seconds = expanded(0, 59, &self.second);
        for &h in &hours {
            if same_day && h < now_h {
                continue;
            }
            for &m in &minutes {
                if same_day && h == now_h && m < now_m {
                    continue;
                }
                for &s in &seconds {
                    if same_day && h == now_h && m == now_m && s <= now_s {
                        continue;
                    }
                    return NaiveTime::from_hms_opt(h, m, s);
                }
            }
        }
        None
    }

    fn time_before(&self, now: NaiveDateTime, day: NaiveDate) -> Option<NaiveTime> {
        let same_day = day == now.date();
        let now_t = now.time();
        for &h in expanded(0, 23, &self.hour).iter().rev() {
            for &m in expanded(0, 59, &self.minute).iter().rev() {
                for &s in expanded(0, 59, &self.second).iter().rev() {
                    let t = NaiveTime::from_hms_opt(h, m, s)?;
                    if same_day && t >= now_t {
                        continue;
                    }
                    return Some(t);
                }
            }
        }
        None
    }
}

/// systemd shorthand calendar expressions.
fn parse_shorthand(s: &str) -> Option<CalendarSpec> {
    let t = s.to_ascii_lowercase();
    let any: BTreeSet<u32> = BTreeSet::new();
    match t.as_str() {
        "minutely" => Some(CalendarSpec {
            weekdays: None,
            year: any.clone(),
            month: any.clone(),
            day: any.clone(),
            hour: any.clone(),
            minute: any.clone(),
            second: BTreeSet::from([0]),
        }),
        "hourly" => Some(CalendarSpec {
            weekdays: None,
            year: any.clone(),
            month: any.clone(),
            day: any.clone(),
            hour: any.clone(),
            minute: BTreeSet::from([0]),
            second: BTreeSet::from([0]),
        }),
        "daily" => Some(CalendarSpec {
            weekdays: None,
            year: any.clone(),
            month: any.clone(),
            day: any.clone(),
            hour: BTreeSet::from([0]),
            minute: BTreeSet::from([0]),
            second: BTreeSet::from([0]),
        }),
        "weekly" => Some(CalendarSpec {
            weekdays: Some(BTreeSet::from([1])),
            year: any.clone(),
            month: any.clone(),
            day: any.clone(),
            hour: BTreeSet::from([0]),
            minute: BTreeSet::from([0]),
            second: BTreeSet::from([0]),
        }),
        "monthly" => Some(CalendarSpec {
            weekdays: None,
            year: any.clone(),
            month: any.clone(),
            day: BTreeSet::from([1]),
            hour: BTreeSet::from([0]),
            minute: BTreeSet::from([0]),
            second: BTreeSet::from([0]),
        }),
        "yearly" | "annually" => Some(CalendarSpec {
            weekdays: None,
            year: any.clone(),
            month: BTreeSet::from([1]),
            day: BTreeSet::from([1]),
            hour: BTreeSet::from([0]),
            minute: BTreeSet::from([0]),
            second: BTreeSet::from([0]),
        }),
        "quarterly" => Some(CalendarSpec {
            weekdays: None,
            year: any.clone(),
            month: BTreeSet::from([1, 4, 7, 10]),
            day: BTreeSet::from([1]),
            hour: BTreeSet::from([0]),
            minute: BTreeSet::from([0]),
            second: BTreeSet::from([0]),
        }),
        "semi-annually" => Some(CalendarSpec {
            weekdays: None,
            year: any.clone(),
            month: BTreeSet::from([1, 7]),
            day: BTreeSet::from([1]),
            hour: BTreeSet::from([0]),
            minute: BTreeSet::from([0]),
            second: BTreeSet::from([0]),
        }),
        _ => None,
    }
}

/// Parse a date token (`*-*-*`, `*-12-24`, `2026-01-01`, `1..15-*-*` …).
fn parse_date(tok: &str) -> Result<(CalendarPart, CalendarPart, CalendarPart), String> {
    let parts: Vec<&str> = tok.split('-').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(sysa::l10n::fmt(
            sysa::l10n::t_("invalid date '{tok}'"),
            &[("tok", &tok.to_string())],
        ));
    }
    if parts.len() == 2 {
        let month = parse_comp(parts[0], 1, 12)?;
        let day = parse_comp(parts[1], 1, 31)?;
        return Ok((BTreeSet::new(), month, day));
    }
    let year = parse_comp(parts[0], 1970, 3000)?;
    let month = parse_comp(parts[1], 1, 12)?;
    let day = parse_comp(parts[2], 1, 31)?;
    Ok((year, month, day))
}

/// Parse a time token (`09:00`, `9..17:00`, `*:*:*`).  Missing components
/// default to zero only when the token has fewer than three parts and does
/// not use a wildcard hour; otherwise the standard domain rule applies.
/// The second return value reports whether the time was completely defaulted.
fn parse_time(tok: &str) -> Result<(CalendarPart, CalendarPart, CalendarPart), String> {
    let parts: Vec<&str> = tok.split(':').collect();
    if parts.len() > 3 {
        return Err(sysa::l10n::fmt(
            sysa::l10n::t_("invalid time '{tok}'"),
            &[("tok", &tok.to_string())],
        ));
    }
    let hour = parse_comp(parts[0], 0, 23)?;
    let minute = if parts.len() >= 2 {
        parse_comp(parts[1], 0, 59)?
    } else {
        BTreeSet::from([0])
    };
    let second = if parts.len() >= 3 {
        parse_comp(parts[2], 0, 59)?
    } else {
        BTreeSet::from([0])
    };
    Ok((hour, minute, second))
}

// ---------------------------------------------------------------------------
// Schedule computation
// ---------------------------------------------------------------------------

/// Compute the next elapse (plus the responsible directive) for an active
/// timer, or `None` when no elapse remains.
///
/// - Monotonic one-shot triggers (`OnBootSec=`, `OnStartupSec=`,
///   `OnActiveSec=`) fire once at their anchor + offset.
/// - `OnUnitActiveSec=` / `OnUnitInactiveSec=` repeat from the previous
///   elapse (v1: anchored to this worker's own trigger time).
/// - `OnCalendar=` specs repeat forever.
pub fn compute_next_elapse(
    cfg: &TimerConfig,
    last_elapse: Option<u64>,
    now: u64,
    boot_epoch: u64,
    started_epoch: u64,
    activated_epoch: u64,
) -> Option<(u64, &'static str)> {
    let mut candidates: Vec<(u64, &'static str)> = Vec::new();

    // Monotonic one-shot triggers are consumed by the first fire: they only
    // enter the schedule while the timer has never fired yet.
    if last_elapse.is_none() {
        if let Some(v) = cfg.on_active_sec {
            candidates.push((activated_epoch.saturating_add(v as u64), "OnActiveSec"));
        }
        if let Some(v) = cfg.on_boot_sec {
            candidates.push((boot_epoch.saturating_add(v as u64), "OnBootSec"));
        }
        if let Some(v) = cfg.on_startup_sec {
            candidates.push((started_epoch.saturating_add(v as u64), "OnStartupSec"));
        }
    }
    if let Some(v) = cfg.on_unit_active_sec {
        let base = last_elapse.map(|e| e.saturating_add(v as u64))
            .unwrap_or_else(|| activated_epoch.saturating_add(v as u64));
        candidates.push((base, "OnUnitActiveSec"));
    }
    if let Some(v) = cfg.on_unit_inactive_sec {
        let base = last_elapse.map(|e| e.saturating_add(v as u64))
            .unwrap_or_else(|| activated_epoch.saturating_add(v as u64));
        candidates.push((base, "OnUnitInactiveSec"));
    }
    for spec_str in &cfg.on_calendar {
        match CalendarSpec::parse(spec_str) {
            Ok(spec) => {
                if let Some(e) = spec.next_epoch(local_now()) {
                    candidates.push((e, "OnCalendar"));
                }
            }
            Err(e) => {
                tracing::warn!(spec = %spec_str, "Ignored invalid OnCalendar= directive: {e}");
            }
        }
    }

    let min = candidates.iter().min_by_key(|c| c.0)?;
    let mut elapse = min.0;
    // Overdue one-shots fire on the next engine tick.
    if elapse < now {
        elapse = now;
    }
    if cfg.randomized_delay_sec > 0 {
        elapse = elapse.saturating_add(rand_below(cfg.randomized_delay_sec as u64 + 1));
    }
    Some((elapse, min.1))
}

/// When `Persistent=yes` is set, return the epoch of the most recent
/// calendar elapse that occurred *before* `since_epoch` (i.e. while the
/// timer worker / system was not running), so System C can fire an
/// immediate catch-up trigger.  This approximates systemd's persistent
/// timers, which run a missed elapse once the machine is back.
pub fn missed_calendar_elapse(cfg: &TimerConfig, since_epoch: u64) -> Option<u64> {
    let since = Local
        .timestamp_opt(since_epoch as i64, 0)
        .single()
        .map(|dt| dt.naive_local())
        .unwrap_or_else(local_now);
    missed_calendar_at(cfg, local_now(), since)
}

/// Testable core of [`missed_calendar_elapse`]: compare in wall-clock terms
/// so the result is timezone-independent.
fn missed_calendar_at(
    cfg: &TimerConfig,
    now: NaiveDateTime,
    since: NaiveDateTime,
) -> Option<u64> {
    if !cfg.persistent || cfg.on_calendar.is_empty() {
        return None;
    }
    for spec_str in &cfg.on_calendar {
        let Ok(spec) = CalendarSpec::parse(spec_str) else {
            continue;
        };
        let Some(prev) = spec.prev_before(now) else {
            continue;
        };
        if prev >= since {
            continue;
        }
        return ndt_to_epoch(prev);
    }
    None
}

/// Derive the unit a timer activates when `Unit=` is empty:
/// `foo.timer` → `foo.service`.
pub fn default_target_unit(timer_unit: &str) -> String {
    if let Some(base) = timer_unit.strip_suffix(".timer") {
        format!("{base}.service")
    } else {
        format!("{timer_unit}.service")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn ndt(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_time(NaiveTime::from_hms_opt(h, min, s).unwrap())
    }

    #[test]
    fn parse_shorthands() {
        for s in ["minutely", "hourly", "daily", "weekly", "monthly", "yearly", "annually", "quarterly", "semi-annually"] {
            assert!(CalendarSpec::parse(s).is_ok(), "failed to parse {s}");
        }
    }

    #[test]
    fn daily_next_and_prev() {
        let spec = CalendarSpec::parse("*-*-* 00:00:00").unwrap();
        // Thursday 2026-08-06
        let noon = ndt(2026, 8, 6, 12, 0, 0);
        assert_eq!(spec.next_after(noon).unwrap(), ndt(2026, 8, 7, 0, 0, 0));
        assert_eq!(spec.prev_before(noon).unwrap(), ndt(2026, 8, 6, 0, 0, 0));
    }

    #[test]
    fn weekday_range() {
        let spec = CalendarSpec::parse("Mon..Fri 09:00:00").unwrap();
        // Friday 2026-08-07 10:00 → next Monday.
        let friday = ndt(2026, 8, 7, 10, 0, 0);
        assert_eq!(spec.next_after(friday).unwrap(), ndt(2026, 8, 10, 9, 0, 0));
    }

    #[test]
    fn wrapping_weekday_range() {
        let spec = CalendarSpec::parse("Fri..Mon 12:00").unwrap();
        let thursday = ndt(2026, 8, 6, 12, 0, 0);
        assert_eq!(spec.next_after(thursday).unwrap(), ndt(2026, 8, 7, 12, 0, 0));
    }

    #[test]
    fn hourly_event() {
        let spec = CalendarSpec::parse("*:00").unwrap();
        let start = ndt(2026, 8, 6, 12, 30, 0);
        assert_eq!(spec.next_after(start).unwrap(), ndt(2026, 8, 6, 13, 0, 0));
    }

    #[test]
    fn every_second() {
        let spec = CalendarSpec::parse("*-*-* *:*:*").unwrap();
        let start = ndt(2026, 8, 6, 12, 0, 0);
        assert_eq!(spec.next_after(start).unwrap(), ndt(2026, 8, 6, 12, 0, 1));
    }

    #[test]
    fn hour_range() {
        let spec = CalendarSpec::parse("9..17:00").unwrap();
        let now = ndt(2026, 8, 6, 10, 30, 0);
        assert_eq!(spec.next_after(now).unwrap(), ndt(2026, 8, 6, 11, 0, 0));
        let after = ndt(2026, 8, 6, 17, 30, 0);
        assert_eq!(spec.next_after(after).unwrap(), ndt(2026, 8, 7, 9, 0, 0));
    }

    #[test]
    fn fixed_date() {
        let spec = CalendarSpec::parse("2026-12-24 18:00:00").unwrap();
        let now = ndt(2026, 8, 6, 0, 0, 0);
        assert_eq!(spec.next_after(now).unwrap(), ndt(2026, 12, 24, 18, 0, 0));
        assert_eq!(spec.next_after(ndt(2026, 12, 24, 19, 0, 0)), None);
    }

    #[test]
    fn month_day() {
        let spec = CalendarSpec::parse("12-25 00:00").unwrap();
        let now = ndt(2026, 8, 6, 0, 0, 0);
        assert_eq!(spec.next_after(now).unwrap(), ndt(2026, 12, 25, 0, 0, 0));
    }

    fn tcfg(
        on_boot: Option<u32>,
        on_active: Option<u32>,
        calendar: Vec<String>,
    ) -> TimerConfig {
        TimerConfig {
            on_active_sec: on_active,
            on_boot_sec: on_boot,
            on_startup_sec: None,
            on_unit_active_sec: None,
            on_unit_inactive_sec: None,
            on_calendar: calendar,
            accuracy_sec: 60,
            randomized_delay_sec: 0,
            unit: String::new(),
            persistent: false,
        }
    }

    #[test]
    fn monotonic_takes_earliest() {
        let cfg = tcfg(Some(100), Some(50), vec![]);
        // boot at 0, activated at 0: on_boot fires at 100s, on_active at 50s.
        let (elapse, reason) = compute_next_elapse(&cfg, None, 0, 0, 0, 0).unwrap();
        assert_eq!(elapse, 50); // on_active wins (sooner)
        assert_eq!(reason, "OnActiveSec");
    }

    #[test]
    fn monotonic_overdue_clamped_to_now() {
        let cfg = tcfg(None, Some(10), vec![]);
        // activated 4000s before "now": the elapse is overdue → clamp to now.
        let (elapse, _) = compute_next_elapse(&cfg, None, 5000, 0, 0, 1000).unwrap();
        assert!(elapse >= 5000);
        // activated in the future: elapse is still scheduled.
        let (future, _) = compute_next_elapse(&cfg, None, 5000, 0, 0, 6000).unwrap();
        assert_eq!(future, 6010);
    }

    #[test]
    fn repeating_unit_active() {
        let mut cfg = tcfg(None, None, vec![]);
        cfg.on_unit_active_sec = Some(300);
        let first = compute_next_elapse(&cfg, None, 1000, 0, 0, 1000).unwrap();
        assert_eq!(first.0, 1300);
        let second = compute_next_elapse(&cfg, Some(1300), 1350, 0, 0, 1000).unwrap();
        assert_eq!(second.0, 1600);
    }

    #[test]
    fn one_shot_not_rescheduled_after_fire() {
        // Regression: systemd-tmpfiles-clean.timer (OnBootSec=15min +
        // OnUnitActiveSec=1d) used to refire on every engine tick because the
        // already-consumed OnBootSec one-shot was recomputed into the past and
        // clamped to "now".  After the first fire only the repeating trigger
        // may schedule the next elapse.
        let mut cfg = tcfg(Some(900), None, vec![]);
        cfg.on_unit_active_sec = Some(86400);
        // boot at 0, activated at 0, now = 900 (first fire of OnBootSec).
        let (first, reason) = compute_next_elapse(&cfg, None, 900, 0, 0, 0).unwrap();
        assert_eq!(first, 900);
        assert_eq!(reason, "OnBootSec");
        // Next computation happens right after that fire: must be +1d, not now.
        let (second, reason) = compute_next_elapse(&cfg, Some(900), 901, 0, 0, 0).unwrap();
        assert_eq!(second, 900 + 86400);
        assert_eq!(reason, "OnUnitActiveSec");
    }

    #[test]
    fn pure_one_shot_becomes_elapsed() {
        let cfg = tcfg(Some(900), None, vec![]);
        // First elapse is scheduled...
        assert_eq!(compute_next_elapse(&cfg, None, 0, 0, 0, 0).unwrap().0, 900);
        // ...but once fired there is nothing left to schedule.
        assert!(compute_next_elapse(&cfg, Some(900), 901, 0, 0, 0).is_none());
    }

    #[test]
    fn no_trigger_means_none() {
        let cfg = tcfg(None, None, vec![]);
        assert!(compute_next_elapse(&cfg, None, 1000, 0, 0, 1000).is_none());
    }

    #[test]
    fn persistent_miss_detected() {
        // Daily 09:00.  now = 2026-08-07 00:30, previous elapse at
        // 2026-08-06 09:00.  With the worker up since 08-06 08:00 (before the
        // elapse) there is nothing to catch up on.
        let mut cfg = tcfg(None, None, vec!["*-*-* 09:00:00".to_string()]);
        cfg.persistent = true;
        let now = ndt(2026, 8, 7, 0, 30, 0);
        let since_before = ndt(2026, 8, 6, 8, 0, 0);
        assert!(missed_calendar_at(&cfg, now, since_before).is_none());

        // A timer worker that came up only after the elapse (09:30) catches
        // up on the elapse it missed (09:00).
        let since_after = ndt(2026, 8, 6, 9, 30, 0);
        assert!(missed_calendar_at(&cfg, now, since_after).is_some());

        // Non-persistent timers never catch up.
        cfg.persistent = false;
        assert!(missed_calendar_at(&cfg, now, since_after).is_none());
    }

    #[test]
    fn default_target_unit_derivation() {
        assert_eq!(default_target_unit("backup.timer"), "backup.service");
        assert_eq!(default_target_unit("weird"), "weird.service");
    }

    #[test]
    fn rand_below_respects_bound() {
        for _ in 0..1000 {
            assert!(rand_below(10) < 10);
        }
    }

    #[test]
    fn sets_parse() {
        assert_eq!(parse_comp("9..17", 0, 23).unwrap(), BTreeSet::from([9, 10, 11, 12, 13, 14, 15, 16, 17]));
        assert_eq!(parse_comp("*", 0, 23).unwrap(), BTreeSet::new());
        assert!(parse_comp("40", 0, 23).is_err());
    }
}
