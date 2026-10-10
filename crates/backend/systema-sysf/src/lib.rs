//! System F common library.
//!
//! Format-agnostic finder infrastructure: the unified unit intermediate
//! representation ([`ir`]) and the [`Finder`] / [`FinderRegistry`]
//! abstraction.  Format-specific finders (e.g. the systemd finder in the
//! `systema-sysf-systemd` crate) implement [`Finder`] and are discovered by
//! the `systema-sysf` executable from the finder search paths.

pub mod ir;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use ir::UnitIR;

/// A Finder is responsible for discovering and parsing configuration files
/// from a specific init system (systemd, SysV, OpenRC, Runit, etc.),
/// producing unified [`UnitIR`] values.
///
/// Each Finder implementation:
/// 1. Knows where to look for its config files (search paths)
/// 2. Can parse those files into [`UnitIR`]
/// 3. May support runtime re-scanning for new/changed units
#[async_trait::async_trait]
pub trait Finder: Send + Sync {
    /// Human-readable name, e.g. "systemd", "sysv".
    fn name(&self) -> &str;

    /// Discover and parse all units this finder can handle.
    /// Returns a map of unit ID → UnitIR.
    async fn find_all(&self) -> Result<HashMap<String, UnitIR>>;

    /// Find and parse a single named unit.
    /// Returns `None` if the unit is not found.
    async fn find_one(&self, id: &str) -> Result<Option<UnitIR>>;

    /// Optional: re-scan for newly added or removed units.
    /// Returns the delta (added, removed) since the last scan.
    async fn scan_for_changes(&self) -> Result<(Vec<UnitIR>, Vec<String>)> {
        let _ = self.find_all().await?;
        Ok((vec![], vec![]))
    }
}

/// A registry of all available Finder implementations.
///
/// System Allocator uses this to resolve unit lookups across all
/// supported init systems without knowing about individual formats.
#[derive(Default)]
pub struct FinderRegistry {
    finders: Vec<Arc<dyn Finder>>,
}

impl FinderRegistry {
    pub fn new() -> Self {
        FinderRegistry { finders: vec![] }
    }

    pub fn register(&mut self, finder: Arc<dyn Finder>) {
        self.finders.push(finder);
    }

    /// Discover all units across all registered finders.
    pub async fn discover_all(&self) -> Result<HashMap<String, UnitIR>> {
        let mut all = HashMap::new();
        for finder in &self.finders {
            let units = finder.find_all().await?;
            for (id, unit) in units {
                all.entry(id).or_insert(unit);
            }
        }
        Ok(all)
    }

    /// Find a single named unit across all finders (first match wins).
    pub async fn find_one(&self, id: &str) -> Result<Option<UnitIR>> {
        for finder in &self.finders {
            if let Some(unit) = finder.find_one(id).await? {
                return Ok(Some(unit));
            }
        }
        Ok(None)
    }

    pub fn finders(&self) -> &[Arc<dyn Finder>] {
        &self.finders
    }
}
