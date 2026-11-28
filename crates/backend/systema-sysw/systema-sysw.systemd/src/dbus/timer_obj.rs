//! Per-unit `Timer` interface (bridge).
//!
//! SysF is replaceable and need not report timer scheduling state, but
//! `systemctl list-timers` reads these members with fatally-checked
//! `Properties.Get` calls.  Serve systemd's "never scheduled" defaults
//! (`USEC_INFINITY` / `0`) so those queries always resolve.

use zbus::interface;

pub struct TimerObject;

#[interface(name = "org.freedesktop.systemd1.Timer")]
impl TimerObject {
    #[zbus(property)]
    fn next_elapse_u_sec_realtime(&self) -> u64 {
        u64::MAX
    }

    #[zbus(property)]
    fn next_elapse_u_sec_monotonic(&self) -> u64 {
        u64::MAX
    }

    #[zbus(property)]
    fn last_trigger_u_sec(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn last_trigger_u_sec_monotonic(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn result(&self) -> String {
        "success".to_string()
    }
}
