//! cgroup path mapping and value conversions.
//!
//! All functions here are pure and platform-neutral so they can be unit
//! tested without a cgroup filesystem.  They implement the naming rules
//! systemd uses for slices: a slice unit name encodes its position in the
//! hierarchy, with each `-` separating a parent/child level.

use std::fmt::Write;

/// Mount point of the unified cgroup v2 hierarchy.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The root slice unit name (systemd's `-.slice`).
pub const ROOT_SLICE_NAME: &str = "-.slice";

/// The top-level container of all per-user slices (systemd's `user.slice`).
/// It is a static, always-present slice: it exists from boot even before
/// any user logs in, and every `user-<UID>.slice` lives inside it.
pub const USER_SLICE_NAME: &str = "user.slice";

/// The slice unit name of a user's session container (`user-<UID>.slice`).
pub fn user_slice_name(uid: u32) -> String {
    format!("user-{uid}.slice")
}

/// cgroup path of the `user.slice` root (`/sys/fs/cgroup/user.slice`).
pub fn user_slice_root_path() -> String {
    slice_cgroup_path(USER_SLICE_NAME)
}

/// cgroup path of a user's slice.  Per the slice naming rule the leaf is
/// named after the last component only, so `user-1000.slice` lives at
/// `/sys/fs/cgroup/user.slice/1000.slice`.
pub fn user_slice_cgroup_path(uid: u32) -> String {
    slice_cgroup_path(&user_slice_name(uid))
}

/// cgroup path of a user's manager service instance `user@<UID>.service`,
/// which systemd places inside the user's own slice.
pub fn user_manager_cgroup_path(uid: u32) -> String {
    unit_cgroup_path(&user_slice_name(uid), &format!("user@{uid}.service"))
}

/// Split a slice unit name into its hierarchy components.
///
/// `"system.slice"` → `["system"]`, `"foo-bar.slice"` → `["foo", "bar"]`,
/// `"-.slice"` → `[]` (the root slice).
pub fn slice_name_components(name: &str) -> Option<Vec<String>> {
    let stem = name.strip_suffix(".slice")?;
    if stem.is_empty() || stem == "-" {
        return Some(vec![]);
    }
    Some(stem.split('-').map(|s| s.to_string()).collect())
}

/// Resolve the cgroup filesystem path of a slice unit's own cgroup.
///
/// The leaf directory of a nested slice is named after the *last* component
/// only: `foo-bar.slice` lives in `.../foo.slice/bar.slice`.
pub fn slice_cgroup_path(slice_name: &str) -> String {
    if slice_name == ROOT_SLICE_NAME {
        return CGROUP_ROOT.to_string();
    }
    let mut path = CGROUP_ROOT.to_string();
    if let Some(stem) = slice_name.strip_suffix(".slice") {
        for comp in stem.split('-') {
            path.push('/');
            path.push_str(comp);
            path.push_str(".slice");
        }
    } else {
        path.push('/');
        path.push_str(slice_name);
    }
    path
}

/// Resolve the cgroup filesystem path of a non-slice unit (service, scope)
/// placed inside the slice named `slice_name`.
///
/// The unit's cgroup is a child of its parent slice's cgroup and is named
/// after the full unit name (e.g. `system.slice/sshd.service`).
pub fn unit_cgroup_path(slice_name: &str, unit_name: &str) -> String {
    format!("{}/{}", slice_cgroup_path(slice_name), unit_name)
}

/// Parse a `CPUQuota=` value into a percentage as `f64`.
///
/// Accepts `"50"`, `"50%"` and decimal quotas such as `"50.5%"`.
/// `infinity` (no quota) and unparseable input yield `None`.
pub fn parse_cpu_quota_percent(value: &str) -> Option<f64> {
    let s = value.trim();
    if s.is_empty() {
        return None;
    }
    let s = s.strip_suffix('%').unwrap_or(s).trim();
    if s.eq_ignore_ascii_case("infinity") || s.eq_ignore_ascii_case("inf") {
        return None;
    }
    let pct: f64 = s.parse().ok()?;
    if !(pct.is_finite()) || pct <= 0.0 {
        return None;
    }
    Some(pct)
}

/// Parse a `CPUQuotaPeriodSec=` value into microseconds.
///
/// Accepts bare microseconds, `"ms"`, `"s"`, `"us"` suffixes and decimal
/// prefixes (e.g. `"100ms"`, `"0.1s"`, `"100000"`).  Unparseable input
/// yields `None`.
pub fn parse_cpu_period_us(value: &str) -> Option<u64> {
    let s = value.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = if let Some(stripped) = s.strip_suffix("ms") {
        (stripped, 1_000u64)
    } else if let Some(stripped) = s.strip_suffix("us") {
        (stripped, 1u64)
    } else if let Some(stripped) = s.strip_suffix('s') {
        (stripped, 1_000_000u64)
    } else {
        (s, 1u64)
    };
    let n: f64 = num.trim().parse().ok()?;
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    Some((n * mult as f64) as u64)
}

/// Convert a `CPUQuota=` value into the cgroup v2 `cpu.max` payload
/// (`"quota_us period_us"`) with the default 100ms period.
pub fn cpu_quota_to_cpu_max(value: &str) -> Option<String> {
    cpu_quota_to_cpu_max_period(value, super::CPU_MAX_PERIOD_US)
}

/// Convert a `CPUQuota=` value into the cgroup v2 `cpu.max` payload
/// (`"quota_us period_us"`) over the given period in microseconds.
///
/// A percentage is a fraction of one full period: 50% over a 100ms period
/// yields a 50ms quota.
pub fn cpu_quota_to_cpu_max_period(value: &str, period_us: u64) -> Option<String> {
    let pct = parse_cpu_quota_percent(value)?;
    let quota_us = (pct / 100.0 * period_us as f64) as u64;
    if quota_us == 0 {
        return None;
    }
    Some(format!("{quota_us} {period_us}"))
}

/// Split a per-device resource directive (`"DEVICE VALUE"`) into its two
/// whitespace-separated parts.  `DEVICE` may be a `/dev` path or a cgroup
/// v2 `"MAJ:MIN"` id.
pub fn split_device_directive(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let device = parts.next()?;
    let value = parts.next()?;
    if device.is_empty() || value.is_empty() {
        None
    } else {
        Some((device, value))
    }
}

/// Parse a memory size string (systemd `parse_size`) into bytes.
///
/// Supports plain byte counts and the `K`/`M`/`G`/`T` suffixes with decimal
/// prefixes, e.g. `"512M"`, `"1.5G"`, `"1048576"`.
pub fn parse_memory_size(value: &str) -> Option<u64> {
    let s = value.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.as_bytes().last() {
        Some(c) if c.is_ascii_alphabetic() => {
            let (num, suffix) = s.split_at(s.len() - 1);
            let mult = match suffix.to_ascii_uppercase().as_str() {
                "K" => 1u64 << 10,
                "M" => 1u64 << 20,
                "G" => 1u64 << 30,
                "T" => 1u64 << 40,
                _ => return None,
            };
            (num, mult)
        }
        _ => (s, 1),
    };
    let n: f64 = num.trim().parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    Some((n * mult as f64) as u64)
}

/// Render a memory byte count back to a decimal `cpu.max`-style string
/// (used for the memory limits on the cgroup v2 files, which are in bytes).
pub fn bytes_to_string(bytes: u64) -> String {
    let mut buf = String::new();
    let _ = write!(buf, "{bytes}");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_components() {
        assert_eq!(slice_name_components("system.slice"), Some(vec!["system".to_string()]));
        assert_eq!(
            slice_name_components("foo-bar.slice"),
            Some(vec!["foo".to_string(), "bar".to_string()])
        );
        assert_eq!(slice_name_components("-.slice"), Some(vec![]));
        assert_eq!(slice_name_components("nope"), None);
    }

    #[test]
    fn slice_paths() {
        assert_eq!(slice_cgroup_path("system.slice"), "/sys/fs/cgroup/system.slice");
        assert_eq!(
            slice_cgroup_path("foo-bar.slice"),
            "/sys/fs/cgroup/foo.slice/bar.slice"
        );
        assert_eq!(slice_cgroup_path("-.slice"), "/sys/fs/cgroup");
        assert_eq!(
            unit_cgroup_path("system.slice", "sshd.service"),
            "/sys/fs/cgroup/system.slice/sshd.service"
        );
        assert_eq!(
            unit_cgroup_path("system-foo.slice", "sshd.service"),
            "/sys/fs/cgroup/system.slice/foo.slice/sshd.service"
        );
    }

    #[test]
    fn user_slice_paths() {
        assert_eq!(user_slice_name(1000), "user-1000.slice");
        assert_eq!(user_slice_root_path(), "/sys/fs/cgroup/user.slice");
        // user-<UID>.slice is a nested slice under user.slice: the leaf is
        // named after the UID component only.
        assert_eq!(
            user_slice_cgroup_path(1000),
            "/sys/fs/cgroup/user.slice/1000.slice"
        );
        assert_eq!(
            user_manager_cgroup_path(1000),
            "/sys/fs/cgroup/user.slice/1000.slice/user@1000.service"
        );
    }

    #[test]
    fn quota_parsing() {
        assert_eq!(parse_cpu_quota_percent("50%"), Some(50.0));
        assert_eq!(parse_cpu_quota_percent("50"), Some(50.0));
        assert_eq!(parse_cpu_quota_percent("50.5%"), Some(50.5));
        assert_eq!(parse_cpu_quota_percent("infinity"), None);
        assert_eq!(parse_cpu_quota_percent(""), None);
        assert_eq!(parse_cpu_quota_percent("junk"), None);
    }

    #[test]
    fn quota_to_cpu_max() {
        assert_eq!(cpu_quota_to_cpu_max("50%"), Some("50000 100000".to_string()));
        assert_eq!(cpu_quota_to_cpu_max("100%"), Some("100000 100000".to_string()));
        assert_eq!(cpu_quota_to_cpu_max("10.5%"), Some("10500 100000".to_string()));
        assert_eq!(cpu_quota_to_cpu_max("infinity"), None);
    }

    #[test]
    fn quota_to_cpu_max_custom_period() {
        assert_eq!(
            cpu_quota_to_cpu_max_period("50%", 50_000),
            Some("25000 50000".to_string())
        );
        assert_eq!(
            cpu_quota_to_cpu_max_period("100%", 1_000_000),
            Some("1000000 1000000".to_string())
        );
    }

    #[test]
    fn cpu_period_parsing() {
        assert_eq!(parse_cpu_period_us("100ms"), Some(100_000));
        assert_eq!(parse_cpu_period_us("50ms"), Some(50_000));
        assert_eq!(parse_cpu_period_us("1s"), Some(1_000_000));
        assert_eq!(parse_cpu_period_us("250us"), Some(250));
        assert_eq!(parse_cpu_period_us("100000"), Some(100_000));
        assert_eq!(parse_cpu_period_us(""), None);
        assert_eq!(parse_cpu_period_us("junk"), None);
    }

    #[test]
    fn device_directives() {
        assert_eq!(
            split_device_directive("/dev/sda 100"),
            Some(("/dev/sda", "100"))
        );
        assert_eq!(
            split_device_directive("8:0 10M"),
            Some(("8:0", "10M"))
        );
        assert_eq!(split_device_directive(""), None);
        assert_eq!(split_device_directive("/dev/sda"), None);
    }

    #[test]
    fn memory_sizes() {
        assert_eq!(parse_memory_size("512M"), Some(512 * 1024 * 1024));
        assert_eq!(parse_memory_size("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_memory_size("1.5G"), Some(1_610_612_736));
        assert_eq!(parse_memory_size("1048576"), Some(1_048_576));
        assert_eq!(parse_memory_size("2T"), Some(2u64 << 40));
        assert_eq!(parse_memory_size(""), None);
        assert_eq!(parse_memory_size("x"), None);
        assert_eq!(parse_memory_size("-5M"), None);
    }

    #[test]
    fn bytes_to_string_roundtrip() {
        assert_eq!(bytes_to_string(1_048_576), "1048576");
    }
}
