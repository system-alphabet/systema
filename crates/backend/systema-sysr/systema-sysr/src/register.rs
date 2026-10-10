//! Registration of the dynamic user-slice units with System A.
//!
//! System R owns the lifecycle of `user.slice` and `user-<UID>.slice`
//! (created from `user.sessions` accounting), but System A's unit model —
//! `state.units`, D-Bus unit objects, scheduling — only knows units it
//! loaded itself.  Without a definition, a slice System R reports as
//! `active` is invisible to `GetUnit`/`ListUnits` and cannot be scheduled.
//!
//! This module closes that gap through the finder API (the same one System
//! D uses to inject discovered device units): the slice unit definitions are
//! registered into a staging area and committed into System A's model, which
//! registers their D-Bus objects synchronously before the commit returns.
//! Commits are idempotent (System A merges over an existing unit), so they
//! are safe to repeat on every (re)connect and session transition.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use sysa::finder::UnitFinder;
use systema_sysf::ir::{DependencySet, UnitIR, UnitType};
use tokio::sync::Mutex;

/// Staging area under which System R registers its dynamic slice units.
const STAGING_NAME: &str = "systema-sysr/slices";

/// Serialize the register → commit cycle on the shared slice staging area.
///
/// Every `commit_slice` cycle stages a unit into the single fixed-name
/// staging area (`STAGING_NAME`) and commits it.  System A *consumes* the
/// area on a successful commit, so two cycles driven concurrently — the
/// (re)connect registration task and the per-session `user.sessions` tasks
/// both call [`commit_slice`] — must not interleave: if a sibling commit
/// lands between this cycle's `register_units` and `commit_units`, the
/// staging area is already gone and System A refuses the commit with
/// `no staging area for UID ... with name '...'`.  Holding the lock across
/// the whole cycle makes each commit atomic.
static STAGING_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn staging_lock() -> &'static Mutex<()> {
    STAGING_LOCK.get_or_init(|| Mutex::new(()))
}

/// Description of the static user container (systemd: "User and Session
/// Slice").
pub fn static_user_slice_description() -> String {
    sysa::l10n::t_("User and Session Slice")
}

/// The description systemd uses for a per-user slice.
pub fn user_slice_description(uid: u32) -> String {
    sysa::l10n::fmt(
        sysa::l10n::t_("User Slice of UID {uid}"),
        &[("uid", &uid.to_string())],
    )
}

/// Build the minimal `UnitIR` of a slice unit definition.
pub fn user_slice_ir(unit_name: &str, description: &str) -> UnitIR {
    UnitIR {
        id: unit_name.to_string(),
        unit_type: Some(UnitType::Slice),
        description: Some(description.to_string()),
        source_format: Some("dynamic".to_string()),
        source_path: None,
        aliases: Vec::new(),
        slice: None,
        dependencies: None,
        service: None,
        mount: None,
        automount: None,
        timer: None,
        socket: None,
        resource_control: None,
        conditions: None,
        asserts: None,
        wanted_by: None,
        required_by: None,
    }
}

/// Register and commit one slice unit definition with System A.
///
/// Idempotent: committing a unit that already exists merges the (identical)
/// definition.  Fails only on transport errors or an explicit refusal from
/// System A.
pub async fn commit_slice(unit_name: &str, description: &str) -> Result<()> {
    let client = UnitFinder::new();
    commit_slice_with(&client, unit_name, description).await
}

/// Serialized register → commit cycle of one slice unit against `client`.
///
/// The caller-visible [`commit_slice`] wrapper falls through to here; the
/// staging lock is held across the whole cycle (see [`STAGING_LOCK`]).
async fn commit_slice_with(client: &UnitFinder, unit_name: &str, description: &str) -> Result<()> {
    let _serial = staging_lock().lock().await;
    let units = HashMap::from([(unit_name.to_string(), user_slice_ir(unit_name, description))]);
    let json =
        serde_json::to_vec(&units).context(sysa::l10n::t_("Cannot serialise slice units"))?;

    let reg = client
        .register_units(STAGING_NAME, json)
        .await
        .context(sysa::l10n::t_("Cannot register slice units with System A"))?;
    if !reg.success {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("System A refused the registration: {message}"),
            &[("message", &(reg.message).to_string())]
        ));
    }
    let commit = client
        .commit_units(STAGING_NAME)
        .await
        .context(sysa::l10n::t_("Cannot commit slice units with System A"))?;
    if !commit.success {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("System A refused the commit: {message}"),
            &[("message", &(commit.message).to_string())]
        ));
    }
    Ok(())
}

/// The unit name of the parent slice of `name`, or `None` when the name is
/// not a valid slice name at all.
///
/// Mirrors systemd's `slice_build_parent_slice()`: the parent is the prefix
/// of the slice name before its last `-` (e.g. `user-0.slice` →
/// `user.slice`); a slice without a `-` prefix hangs directly off the root
/// slice (`-.slice`).
fn parent_slice_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".slice")?;
    if stem.is_empty() {
        return None;
    }
    match stem.rfind('-') {
        Some(idx) if idx > 0 => Some(format!("{}.slice", &stem[..idx])),
        _ => Some(crate::register::ROOT_SLICE_NAME.to_string()),
    }
}

/// The root slice every parent chain terminates at.
const ROOT_SLICE_NAME: &str = "-.slice";

/// Synthesize the definition of `unit_name` (and, transitively, of every
/// ancestor up to — but excluding — the root slice) from the slice name.
///
/// This is the `unit.define` handler's knowledge: System R owns slice
/// definitions and can synthesize any legal slice name without a disk unit
/// file — the counterpart of systemd's `slice_load` materializing the
/// parent chain on demand.  Returns `None` when the name is not a legal
/// slice name (the protocol is deliberately not used for services, mounts,
/// and other static unit types).
///
/// Every synthesized definition declares its parent through both
/// `[Unit] Slice=` and a `Requires=` + `After=` dependency pair, so the
/// System A planner pulls the whole chain into the transaction exactly like
/// systemd's implicit `UNIT_IN_SLICE` dependency does.
pub fn synthesize_slice_chain(unit_name: &str) -> Option<Vec<UnitIR>> {
    if !unit_name.ends_with(".slice") || unit_name.contains('@') || unit_name == ROOT_SLICE_NAME {
        return None;
    }
    let mut chain = Vec::new();
    let mut current = unit_name.to_string();
    loop {
        let parent = parent_slice_name(&current)?;
        chain.push(slice_ir_for(&current, &parent));
        if parent == ROOT_SLICE_NAME {
            break;
        }
        current = parent;
    }
    Some(chain)
}

/// The description systemd uses for a synthesized slice (best effort).
fn slice_description(name: &str) -> Option<String> {
    if name == "user.slice" {
        return Some(static_user_slice_description());
    }
    sysa::unit_name::parse_user_slice_uid(name).map(user_slice_description)
}

/// Build the `UnitIR` of one slice unit definition with its parent declared
/// as an implicit dependency.
fn slice_ir_for(unit_name: &str, parent: &str) -> UnitIR {
    UnitIR {
        id: unit_name.to_string(),
        unit_type: Some(UnitType::Slice),
        description: slice_description(unit_name),
        source_format: Some("dynamic".to_string()),
        source_path: None,
        aliases: Vec::new(),
        slice: Some(parent.to_string()),
        dependencies: Some(DependencySet {
            requires: HashSet::from([parent.to_string()]),
            after: HashSet::from([parent.to_string()]),
            ..Default::default()
        }),
        service: None,
        mount: None,
        automount: None,
        timer: None,
        socket: None,
        resource_control: None,
        conditions: None,
        asserts: None,
        wanted_by: None,
        required_by: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    use prost::Message as ProstMessage;
    use sysa::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
    use sysa::proto::{CommitUnits, RegisterUnits, UnitRegistrationAck};
    use tokio::net::{UnixListener, UnixStream};

    #[test]
    fn slice_ir_shape() {
        let ir = user_slice_ir("user-1000.slice", &user_slice_description(1000));
        assert_eq!(ir.id, "user-1000.slice");
        assert_eq!(ir.unit_type, Some(UnitType::Slice));
        assert_eq!(ir.description.as_deref(), Some("User Slice of UID 1000"));
        assert_eq!(ir.source_format.as_deref(), Some("dynamic"));
    }

    #[test]
    fn parent_of_user_slice_is_user_slice() {
        assert_eq!(parent_slice_name("user-1000.slice"), Some("user.slice".to_string()));
        assert_eq!(parent_slice_name("user-0.slice"), Some("user.slice".to_string()));
        // No "-" prefix → hangs off the root slice.
        assert_eq!(parent_slice_name("user.slice"), Some("-".to_string() + ".slice"));
        assert_eq!(parent_slice_name("system.slice"), Some("-".to_string() + ".slice"));
        assert_eq!(parent_slice_name("foo-bar.slice"), Some("foo.slice".to_string()));
        // Malformed names.
        assert_eq!(parent_slice_name("user-1000.service"), None);
        assert_eq!(parent_slice_name(".slice"), None);
        assert_eq!(parent_slice_name("no-suffix"), None);
    }

    #[test]
    fn synthesize_user_slice_chain() {
        let chain = synthesize_slice_chain("user-1000.slice").expect("user slice synthesizes");
        let ids: Vec<&str> = chain.iter().map(|ir| ir.id.as_str()).collect();
        // user-1000.slice → user.slice, stopping before the root slice
        // (which System A keeps as a perpetual unit).
        assert_eq!(ids, vec!["user-1000.slice", "user.slice"]);

        let user = &chain[0];
        assert_eq!(user.unit_type, Some(UnitType::Slice));
        assert_eq!(user.slice.as_deref(), Some("user.slice"));
        assert_eq!(user.source_format.as_deref(), Some("dynamic"));
        assert_eq!(user.description.as_deref(), Some("User Slice of UID 1000"));
        let deps = user.dependencies.as_ref().expect("parent declared as dependency");
        assert!(deps.requires.contains("user.slice"));
        assert!(deps.after.contains("user.slice"));

        let container = &chain[1];
        assert_eq!(container.slice.as_deref(), Some("-.slice"));
        let static_desc = static_user_slice_description();
        assert_eq!(container.description.as_deref(), Some(static_desc.as_str()));
    }

    #[test]
    fn synthesize_arbitrary_slice_hangs_off_root() {
        let chain = synthesize_slice_chain("foo.slice").expect("plain slice synthesizes");
        let ids: Vec<&str> = chain.iter().map(|ir| ir.id.as_str()).collect();
        assert_eq!(ids, vec!["foo.slice"]);
        assert_eq!(chain[0].slice.as_deref(), Some("-.slice"));
        assert_eq!(chain[0].description, None);
    }

    #[test]
    fn synthesize_rejects_non_slices() {
        assert!(synthesize_slice_chain("nginx.service").is_none());
        assert!(synthesize_slice_chain("user@1000.service").is_none());
        assert!(synthesize_slice_chain("foo@bar.slice").is_none());
        // The root slice is perpetual in System A; never synthesized.
        assert!(synthesize_slice_chain("-.slice").is_none());
        assert!(synthesize_slice_chain(".slice").is_none());
        assert!(synthesize_slice_chain("no-suffix").is_none());
        // Any other legal slice name synthesizes (description best-effort).
        let chain = synthesize_slice_chain("user-abc.slice").expect("arbitrary slice synthesizes");
        assert_eq!(chain[0].description, None);
        assert_eq!(chain[0].slice.as_deref(), Some("user.slice"));
    }

    /// A minimal in-process stub of System A's finder RPC, mirroring its
    /// staging-area semantics: `register_units` merges into a fixed-name
    /// area (git-index style), `commit_units` publishes the area's units and
    /// *consumes* it, and a commit of an already-consumed area is refused
    /// with `no staging area ...`.  System R's reconnect task and its
    /// per-session tasks drive concurrent register→commit cycles over the
    /// same name (`systema-sysr/slices`); the staging lock in
    /// [`commit_slice_with`] is what keeps those cycles from interleaving
    /// and tripping the refusal.
    async fn serve_finder_connection(
        stream: UnixStream,
        areas: Arc<Mutex<HashMap<(u32, String), HashMap<String, UnitIR>>>>,
        committed: Arc<Mutex<HashSet<String>>>,
    ) -> Result<()> {
        let mut framed = frame_stream(stream);
        let Some(env) = recv_envelope(&mut framed).await? else {
            return Ok(());
        };
        let ack = match env.method.as_str() {
            "finder.register_units" => {
                let msg = RegisterUnits::decode(env.payload.as_slice())?;
                let units: HashMap<String, UnitIR> =
                    serde_json::from_slice(&msg.units_json).context("bad units_json")?;
                let mut areas = areas.lock().unwrap();
                let area = areas.entry((0, msg.name.clone())).or_default();
                area.extend(units);
                UnitRegistrationAck {
                    success: true,
                    message: "staged".to_string(),
                    unit_count: area.len() as u32,
                }
            }
            "finder.commit_units" => {
                let msg = CommitUnits::decode(env.payload.as_slice())?;
                let area = areas.lock().unwrap().remove(&(msg.uid, msg.name.clone()));
                match area {
                    Some(units) => {
                        committed.lock().unwrap().extend(units.keys().cloned());
                        UnitRegistrationAck {
                            success: true,
                            message: "committed".to_string(),
                            unit_count: units.len() as u32,
                        }
                    }
                    None => UnitRegistrationAck {
                        success: false,
                        message: format!("no staging area for UID {} with name '{}'", msg.uid, msg.name),
                        unit_count: 0,
                    },
                }
            }
            other => anyhow::bail!("unexpected finder method {other}"),
        };
        let ack_env = make_envelope(env.request_id, "system-a", "system-f", "finder.ack", ack)?;
        send_envelope(&mut framed, &ack_env).await?;
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_commit_slice_cycles_do_not_race() {
        let dir = std::env::temp_dir().join(format!("sysr-finder-mock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("systema.sock");
        let _ = std::fs::remove_file(&sock);

        let listener = UnixListener::bind(&sock).unwrap();
        let areas: Arc<Mutex<HashMap<(u32, String), HashMap<String, UnitIR>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let committed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        let server_areas = areas.clone();
        let server_committed = committed.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let areas = server_areas.clone();
                let committed = server_committed.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_finder_connection(stream, areas, committed).await {
                        eprintln!("mock finder error: {e}");
                    }
                });
            }
        });

        // Drive many registers+commits concurrently — the exact shape of
        // System R's reconnect task racing its per-session tasks.
        let client = Arc::new(UnitFinder::with_socket(sock.to_string_lossy().to_string()));
        let mut tasks = Vec::new();
        for i in 0..8 {
            let client = client.clone();
            tasks.push(tokio::spawn(async move {
                commit_slice_with(
                    &client,
                    &format!("user-{i}.slice"),
                    &format!("User Slice of UID {i}"),
                )
                .await
            }));
        }
        let mut refused = 0usize;
        for task in tasks {
            if let Err(e) = task.await.unwrap() {
                if e.to_string().contains("refused the commit") {
                    refused += 1;
                }
            }
        }

        server.abort();

        assert_eq!(refused, 0, "a concurrent commit was refused (staging race)");
        for i in 0..8 {
            assert!(
                committed.lock().unwrap().contains(&format!("user-{i}.slice")),
                "user-{i}.slice was never committed"
            );
        }
        let _ = std::fs::remove_file(&sock);
    }
}
