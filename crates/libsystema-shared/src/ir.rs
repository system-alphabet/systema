//! Unified unit intermediate representation, abstracted across all init
//! systems (systemd, SysV, OpenRC, Runit, ...).
//!
//! Lives in the IPC kernel because it is the payload the unit-declare and
//! staging flows exchange: the model is serialized to JSON bytes and
//! carried inside the protobuf `units_json` fields of the wire messages
//! (`proto/common.proto`, `proto/workload.proto`, `proto/control.proto`).
//!
//! Only the *common* fields are typed here.  The type-specific section
//! (`[Service]`, `[Mount]`, `[Timer]`, ...) travels as a flattened directive
//! map ([`UnitIR::config`]): keys are the directive names exactly as written
//! in the unit file, values are JSON strings for single-valued directives and
//! arrays of strings for repeated ones.  The owning section is implied by
//! [`UnitIR::unit_type`], so no section name is part of the key.  Parsing and
//! interpreting those directives belongs to the component that consumes
//! them (the allocator, or the worker that owns the unit type).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Type-specific unit directives, keyed by their unit-file names.
///
/// A single-valued directive (`Type=notify`) maps to a JSON string; a
/// repeated directive (`ExecStartPre=...`) maps to an array of strings.
/// Values keep the unit-file spelling (modifier prefixes included), so the
/// consumer parses them the same way it would parse the file itself.
pub type ConfigMap = serde_json::Map<String, serde_json::Value>;

/// Typed accessors for [`ConfigMap`].
///
/// Consumers should never pattern-match JSON values by hand: these helpers
/// accept both the string form a unit file uses and the JSON scalar form a
/// producer may emit for numeric or boolean directives.
pub mod config {
    use anyhow::Context;
    use serde_json::Value;

    use super::ConfigMap;
    use crate::error_code::{ErrorCode, SysError};

    /// Decode a `config_json` payload into a directive map.
    ///
    /// An empty payload means "no type-specific configuration".
    pub fn parse_json(bytes: &[u8]) -> anyhow::Result<ConfigMap> {
        if bytes.is_empty() {
            return Ok(ConfigMap::new());
        }
        serde_json::from_slice(bytes).context(SysError::new(ErrorCode::UnitConfigDecodeFailed))
    }

    /// Encode a directive map for the `config_json` wire field.
    ///
    /// A `serde_json::Value` map is always serializable, so the only failure
    /// mode is an internal serialization bug; it degrades to an empty payload.
    pub fn to_json_bytes(map: &ConfigMap) -> Vec<u8> {
        serde_json::to_vec(map).unwrap_or_default()
    }

    fn as_string(value: &Value) -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// A single directive value.  For a repeated directive this returns the
    /// first element, which is what most scalar consumers want.
    pub fn get_string(map: &ConfigMap, key: &str) -> Option<String> {
        map.get(key).and_then(as_string)
    }

    /// A single directive value, falling back to `default` when absent.
    pub fn get_string_or(map: &ConfigMap, key: &str, default: &str) -> String {
        get_string(map, key).unwrap_or_else(|| default.to_string())
    }

    /// Every value of a directive, in file order.  A single-valued entry is
    /// returned as a one-element vector.
    pub fn get_strings(map: &ConfigMap, key: &str) -> Vec<String> {
        match map.get(key) {
            Some(Value::Array(values)) => values.iter().filter_map(as_string).collect(),
            Some(value) => as_string(value).into_iter().collect(),
            None => Vec::new(),
        }
    }

    /// A boolean directive (`yes`/`no`, `true`/`false`, `1`/`0`, `on`/`off`).
    ///
    /// Absent or unparseable values are `false`, which matches the systemd
    /// default for every boolean directive the IR carries.
    pub fn get_bool(map: &ConfigMap, key: &str) -> bool {
        match map.get(key) {
            Some(Value::Bool(value)) => *value,
            Some(value) => as_string(value)
                .map(|value| {
                    matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "1" | "yes" | "true" | "on"
                    )
                })
                .unwrap_or(false),
            None => false,
        }
    }

    /// An unsigned integer directive.  `None` distinguishes "absent" from
    /// an explicit zero.
    pub fn get_u32(map: &ConfigMap, key: &str) -> Option<u32> {
        match map.get(key)? {
            Value::Number(number) => number.as_u64().and_then(|value| value.try_into().ok()),
            value => as_string(value)?.trim().parse().ok(),
        }
    }

    /// An unsigned integer directive, falling back to `default`.
    pub fn get_u32_or(map: &ConfigMap, key: &str, default: u32) -> u32 {
        get_u32(map, key).unwrap_or(default)
    }

    /// A signed integer directive, falling back to `default`.
    pub fn get_i32_or(map: &ConfigMap, key: &str, default: i32) -> i32 {
        match map.get(key) {
            Some(Value::Number(number)) => number
                .as_i64()
                .and_then(|value| value.try_into().ok())
                .unwrap_or(default),
            Some(value) => as_string(value)
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(default),
            None => default,
        }
    }

    /// A file-mode directive (`0755`, `0666`, ...), parsed as octal.
    ///
    /// A JSON number is taken as the already-resolved mode.
    pub fn get_mode(map: &ConfigMap, key: &str) -> Option<u32> {
        match map.get(key)? {
            Value::Number(number) => number.as_u64().and_then(|value| value.try_into().ok()),
            value => {
                let text = as_string(value)?;
                let text = text.trim();
                let text = text
                    .strip_prefix("0o")
                    .or_else(|| text.strip_prefix("0O"))
                    .unwrap_or(text);
                u32::from_str_radix(text, 8)
                    .ok()
                    .or_else(|| text.parse().ok())
            }
        }
    }
}

/// Unified unit type, abstracted across all init systems.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnitType {
    Service,
    Target,
    Mount,
    Automount,
    Timer,
    Socket,
    Slice,
    Scope,
    Swap,
    Path,
    Device,
    Power,
    /// Custom/unknown unit type from a foreign init system.
    Other(String),
}

impl UnitType {
    pub fn as_str(&self) -> &str {
        match self {
            UnitType::Service => "service",
            UnitType::Target => "target",
            UnitType::Mount => "mount",
            UnitType::Automount => "automount",
            UnitType::Timer => "timer",
            UnitType::Socket => "socket",
            UnitType::Slice => "slice",
            UnitType::Scope => "scope",
            UnitType::Swap => "swap",
            UnitType::Path => "path",
            UnitType::Device => "device",
            UnitType::Power => "power",
            UnitType::Other(s) => s.as_str(),
        }
    }
}

/// Dependency relationships between units.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DependencySet {
    /// Units that must start before this one.
    pub after: HashSet<String>,
    /// Units that should start before this one.
    pub before: HashSet<String>,
    /// Hard dependencies: these units must be active.
    pub requires: HashSet<String>,
    /// Soft dependencies: start if possible, ignore on failure.
    pub wants: HashSet<String>,
    /// Conflicting units: stop these when this unit starts.
    pub conflicts: HashSet<String>,
    /// Lifecycle binding: stop this unit if the bound unit stops.
    pub binds_to: HashSet<String>,
    /// Like Requires, but the dep must already be active.
    pub requisite: HashSet<String>,
    /// Units that are part of this unit (stop propagation).
    pub part_of: HashSet<String>,
    /// Units to keep continuously activated.
    pub upholds: HashSet<String>,
    /// Trigger targets on success.
    pub on_success: HashSet<String>,
    /// Trigger targets on failure.
    pub on_failure: HashSet<String>,
    /// Reload propagation targets.
    pub propagates_reload_to: HashSet<String>,
    /// Action to perform when the unit succeeds (e.g. `poweroff-force`).
    pub success_action: String,
    pub default_dependencies: bool,
}

/// Template metadata for a template definition or one of its instances.
///
/// A template unit (`getty@.service`) is a definition, not a runnable unit:
/// its values keep the instance specifiers (`%i`, `%I`) unexpanded so that a
/// consumer can materialize any instance from it.  An instance
/// (`getty@tty3.service`) records the parameters that were substituted and the
/// template it specializes.  A producer that reports an instance has already
/// merged that instance's own drop-ins into it, which is what makes a reported
/// instance win over a template instantiation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateInfo {
    /// True when this IR *is* the template definition (`getty@.service`),
    /// rather than an instance of one.
    #[serde(default)]
    pub definition: bool,

    /// The template this IR specializes (`getty@.service` for
    /// `getty@tty3.service`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,

    /// The parameters substituted for the template, in order.  systemd carries
    /// at most one (the `%i` instance string); a vector is the reserved shape
    /// for backends that template on more than one value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<String>,
}

/// The unified intermediate representation (IR) for a single unit.
///
/// Every Finder (systemd, SysV, OpenRC, Runit, etc.) produces this type.
/// System Allocator works exclusively with `UnitIR`, never with raw config files.
///
/// Only `id` is mandatory.  Every other field is optional so a commit can be
/// a *partial* update: when the unit already exists in System A's runtime
/// cache, only the fields provided here are overwritten and everything else
/// is left untouched (e.g. a mount-table commit carrying only `config`
/// never clobbers the unit's `description`).  When the unit does not
/// exist yet, a new one is created from the provided fields; missing
/// required fields (`unit_type`) then cause an error returned to the worker
/// that submitted the commit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnitIR {
    /// Canonical unit identifier, e.g. "nginx.service", "network", "sshd".
    pub id: String,
    pub unit_type: Option<UnitType>,
    /// Human-readable description.
    pub description: Option<String>,
    /// Source format, e.g. "systemd", "sysv", "openrc", "runit".
    pub source_format: Option<String>,
    /// The file path this unit was loaded from, if applicable.
    pub source_path: Option<String>,

    /// Alias names that refer to this unit (e.g. `display-manager.service`
    /// for `lightdm.service`).  Aliases come from `[Install] Alias=` lines
    /// and from unit-file symlinks (a symlink's basename aliases the
    /// canonical unit it points at).
    ///
    /// The systemd finder no longer reports aliases here: it resolves them
    /// against the filesystem (which name actually exists, and in which
    /// search-path directory) and reports one [`Self::alias_of`] record per
    /// alias name instead.  The field stays for producers that only declare
    /// aliases.
    #[serde(default)]
    pub aliases: Vec<String>,

    /// Set on an *alias record*: this IR does not define a unit, it declares
    /// that `id` is another name for the canonical unit `alias_of`.
    ///
    /// Alias ownership is decided by the Finder, which is the component that
    /// sees the filesystem and therefore knows whether a name is materialised
    /// as a symlink or only declared via `[Install] Alias=`, and which
    /// search-path directory wins.  System A only records the mapping: it
    /// never chooses between competing declarations.
    ///
    /// Consumers must not create a unit for an alias record — the alias
    /// shares the target's jobs, state and object identity.  No other field
    /// of an alias record is meaningful.
    #[serde(default)]
    pub alias_of: Option<String>,

    /// `[Unit] Slice=` — the parent slice this unit belongs to (e.g.
    /// `"system.slice"`).  `None` when the directive is absent or empty;
    /// consumers default to `system.slice` at apply time.
    pub slice: Option<String>,

    /// Dependencies on other units.  When provided, the whole dependency
    /// set replaces the previous one.
    pub dependencies: Option<DependencySet>,

    /// Type-specific directives of the unit's own section, flattened.
    ///
    /// The section is implied by [`Self::unit_type`] (`Service` → `[Service]`,
    /// `Mount` → `[Mount]`, ...).  Keys keep the systemd spelling and
    /// capitalization; repeated directives are arrays of strings.  `None`
    /// means "not reported" (a partial update leaves the previous map in
    /// place); an empty map means "reported, but the section is empty".
    ///
    /// The IR deliberately does not type these directives: the component
    /// that owns the unit type parses the keys it knows, so the wire contract
    /// stays exactly as wide as the unit file itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigMap>,

    /// Conditions that must be met for the unit to start.
    pub conditions: Option<Vec<Condition>>,
    /// Asserts that cause hard failure if not met.
    pub asserts: Option<Vec<Condition>>,

    /// Install section: which targets want this unit.
    pub wanted_by: Option<Vec<String>>,
    pub required_by: Option<Vec<String>>,

    /// Set by the Finder: whether the unit is enabled, i.e. whether the unit
    /// file set makes it part of the boot set.
    ///
    /// Enablement is a fact about the filesystem, not about the unit file
    /// body: `[Install] WantedBy=` in [`Self::wanted_by`] is only the recipe
    /// `systemctl enable` follows, never proof that it ran.  Only the Finder
    /// sees the search paths, so it owns the answer (the same way it owns
    /// [`Self::alias_of`]); System A stores and reports it verbatim.
    ///
    /// `None` when the producer does not report enablement (dynamic units,
    /// partial updates).  A producer that reports a unit as enabled must also
    /// be able to report it as disabled: the value describes the file set,
    /// not this single IR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    /// Full systemd `UnitFileState` for this unit, as reported by the Finder:
    /// `"enabled"`, `"enabled-runtime"`, `"linked"`, `"linked-runtime"`,
    /// `"static"`, `"indirect"` or `"disabled"`.
    ///
    /// [`Self::enabled`] is only the coarse "belongs to some target's boot
    /// set" verdict.  The finer distinctions (`/run` links, unit-file links,
    /// installable-but-unlinked, `Also=`-only) are filesystem facts that only
    /// the Finder can observe, so it reports the state explicitly and System
    /// A stores it verbatim.  `None` when the producer does not report it, in
    /// which case System A derives the state from [`Self::enabled`] and the
    /// install information it already has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_file_state: Option<String>,

    /// Template metadata: set on a template definition (`getty@.service`) or
    /// on an instance of one.  `None` for ordinary units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<TemplateInfo>,
}

impl UnitIR {
    /// True when this IR is a template definition, not a runnable unit.
    pub fn is_template(&self) -> bool {
        self.template.as_ref().is_some_and(|t| t.definition)
    }

    /// True when this IR is an instance of a template.
    pub fn is_template_instance(&self) -> bool {
        self.template
            .as_ref()
            .is_some_and(|t| !t.definition && t.of.is_some())
    }

    /// The template this IR specializes, if it is an instance.
    pub fn template_of(&self) -> Option<&str> {
        self.template.as_ref().and_then(|t| t.of.as_deref())
    }

    /// The parameters substituted for the template, empty for non-instances.
    pub fn template_parameters(&self) -> &[String] {
        self.template
            .as_ref()
            .map(|t| t.parameters.as_slice())
            .unwrap_or(&[])
    }
}

/// A condition or assert directive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Condition {
    /// The condition type, e.g. "PathExists", "Virtualization", "Host".
    pub kind: String,
    /// The value to check, possibly negated.
    pub value: String,
    /// Whether the check is negated (prefixed with `!`).
    pub negate: bool,
    /// Whether this is a *triggering* condition (prefixed with `|`).
    ///
    /// systemd semantics: when at least one triggering condition exists, the
    /// unit starts if any triggering condition applies **and** every regular
    /// condition applies.  Without triggering conditions all conditions must
    /// apply (logical AND).
    #[serde(default)]
    pub trigger: bool,
}

impl Condition {
    /// Build a condition from a raw directive value, honoring the `|` trigger
    /// and `!` negation prefixes (`|` comes first, then `!`, per systemd.unit).
    pub fn from_value(kind: &str, value: &str) -> Self {
        let (trigger, rest) = match value.strip_prefix('|') {
            Some(rest) => (true, rest),
            None => (false, value),
        };
        let (negate, val) = match rest.strip_prefix('!') {
            Some(rest) => (true, rest.to_string()),
            None => (false, rest.to_string()),
        };
        Condition {
            kind: kind.to_string(),
            value: val,
            negate,
            trigger,
        }
    }
}

impl DependencySet {
    pub fn is_empty(&self) -> bool {
        self.after.is_empty()
            && self.before.is_empty()
            && self.requires.is_empty()
            && self.wants.is_empty()
            && self.conflicts.is_empty()
            && self.binds_to.is_empty()
            && self.requisite.is_empty()
            && self.part_of.is_empty()
            && self.upholds.is_empty()
            && self.on_success.is_empty()
            && self.on_failure.is_empty()
            && self.propagates_reload_to.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::config;
    use super::{ConfigMap, UnitIR};
    use serde_json::json;

    fn sample_map() -> ConfigMap {
        let mut map = ConfigMap::new();
        map.insert("Type".to_string(), json!("notify"));
        map.insert("ExecStart".to_string(), json!(["/usr/bin/a", "/usr/bin/b"]));
        map.insert("TimeoutStartSec".to_string(), json!(90));
        map.insert("RemainAfterExit".to_string(), json!(true));
        map.insert("SocketMode".to_string(), json!("0666"));
        map
    }

    #[test]
    fn typed_accessors_read_both_strings_and_scalars() {
        let map = sample_map();
        assert_eq!(config::get_string(&map, "Type").as_deref(), Some("notify"));
        assert_eq!(
            config::get_strings(&map, "ExecStart"),
            vec!["/usr/bin/a".to_string(), "/usr/bin/b".to_string()]
        );
        assert_eq!(config::get_u32(&map, "TimeoutStartSec"), Some(90));
        assert!(config::get_bool(&map, "RemainAfterExit"));
        assert_eq!(config::get_mode(&map, "SocketMode"), Some(0o666));
        assert_eq!(config::get_strings(&map, "Missing"), Vec::<String>::new());
    }

    #[test]
    fn config_map_round_trips_through_json_bytes() {
        let map = sample_map();
        let bytes = config::to_json_bytes(&map);
        let decoded = config::parse_json(&bytes).unwrap();
        assert_eq!(decoded, map);
    }

    #[test]
    fn empty_payload_means_empty_config() {
        assert!(config::parse_json(&[]).unwrap().is_empty());
    }

    #[test]
    fn unit_ir_without_config_omits_the_field() {
        let ir = UnitIR {
            id: "example.service".to_string(),
            ..Default::default()
        };
        let value = serde_json::to_value(&ir).unwrap();
        assert!(value.get("config").is_none());
    }
}
