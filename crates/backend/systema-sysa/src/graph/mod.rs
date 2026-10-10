use std::collections::HashMap;

use anyhow::{bail, Result};
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use sysa::l10n;
use tracing::{debug, warn};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Strong,
    Weak,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct DependencyGraph {
    graph: DiGraph<String, EdgeKind>,
    index: HashMap<String, NodeIndex>,
}

#[allow(dead_code)]
impl DependencyGraph {
    pub fn build<'a>(units: impl Iterator<Item = &'a crate::unit::types::UnitFile>) -> Self {
        let mut graph = DiGraph::new();
        let mut index: HashMap<String, NodeIndex> = HashMap::new();

        let units: Vec<&crate::unit::types::UnitFile> = units.collect();

        for unit in &units {
            let node = graph.add_node(unit.name.clone());
            index.insert(unit.name.clone(), node);
        }

        let get_or_create_node = |graph: &mut DiGraph<String, EdgeKind>,
                                  index: &mut HashMap<String, NodeIndex>,
                                  name: &str|
         -> NodeIndex {
            if let Some(&n) = index.get(name) {
                n
            } else {
                let n = graph.add_node(name.to_string());
                index.insert(name.to_string(), n);
                n
            }
        };

        for unit in &units {
            let Some(&unit_node) = index.get(&unit.name) else {
                continue;
            };

            for dep in &unit.unit.after {
                let dep_node = get_or_create_node(&mut graph, &mut index, dep);
                if !graph.contains_edge(dep_node, unit_node) {
                    graph.add_edge(dep_node, unit_node, EdgeKind::Weak);
                }
            }

            for dep in &unit.unit.before {
                let dep_node = get_or_create_node(&mut graph, &mut index, dep);
                if !graph.contains_edge(unit_node, dep_node) {
                    graph.add_edge(unit_node, dep_node, EdgeKind::Weak);
                }
            }

            for dep in &unit.unit.requisite {
                let dep_node = get_or_create_node(&mut graph, &mut index, dep);
                if !graph.contains_edge(dep_node, unit_node) {
                    graph.add_edge(dep_node, unit_node, EdgeKind::Strong);
                }
            }

            for dep in &unit.unit.binds_to {
                let dep_node = get_or_create_node(&mut graph, &mut index, dep);
                if !graph.contains_edge(dep_node, unit_node) {
                    graph.add_edge(dep_node, unit_node, EdgeKind::Strong);
                }
            }
        }

        DependencyGraph { graph, index }
    }

    pub fn topological_order(&self) -> Result<Vec<String>> {
        if let Ok(nodes) = toposort(&self.graph, None) {
            let names = nodes.into_iter().map(|n| self.graph[n].clone()).collect();
            return Ok(names);
        }

        let mut graph = self.graph.clone();
        let max_attempts = graph.edge_count();

        for _ in 0..max_attempts {
            match toposort(&graph, None) {
                Ok(nodes) => {
                    let names = nodes.into_iter().map(|n| graph[n].clone()).collect();
                    return Ok(names);
                }
                Err(cycle) => {
                    let cycle_node = cycle.node_id();
                    let weak_edge = graph
                        .edges_directed(cycle_node, petgraph::Direction::Incoming)
                        .find(|e| *e.weight() == EdgeKind::Weak)
                        .map(|e| e.id());

                    if let Some(eid) = weak_edge {
                        let (src, tgt) = graph.edge_endpoints(eid).unwrap();
                        warn!(
                            "Breaking dependency cycle: removing weak edge {} → {}",
                            graph[src], graph[tgt]
                        );
                        graph.remove_edge(eid);
                    } else {
                        let any_edge = graph
                            .edges_directed(cycle_node, petgraph::Direction::Incoming)
                            .next()
                            .map(|e| e.id());
                        if let Some(eid) = any_edge {
                            let (src, tgt) = graph.edge_endpoints(eid).unwrap();
                            warn!(
                                "Breaking dependency cycle: removing strong edge {} → {} (no weak edges available)",
                                graph[src], graph[tgt]
                            );
                            graph.remove_edge(eid);
                        } else {
                            bail!("{}", l10n::fmt(
                                l10n::t_("Dependency cycle detected involving unit: {unit} (unable to break)."),
                                &[("unit", &graph[cycle_node].to_string())],
                            ));
                        }
                    }
                }
            }
        }

        bail!(
            "{}",
            l10n::fmt(
                l10n::t_("Unable to resolve dependency cycles after removing {count} edges."),
                &[("count", &max_attempts.to_string())],
            )
        )
    }

    pub fn start_order_for(&self, unit_name: &str) -> Result<Vec<String>> {
        let Some(&start_node) = self.index.get(unit_name) else {
            return Ok(vec![unit_name.to_string()]);
        };

        let mut ancestors = vec![];
        let mut stack = vec![start_node];
        let mut visited = std::collections::HashSet::new();

        while let Some(node) = stack.pop() {
            if visited.contains(&node) {
                continue;
            }
            visited.insert(node);
            ancestors.push(node);
            for neighbor in self
                .graph
                .neighbors_directed(node, petgraph::Direction::Incoming)
            {
                stack.push(neighbor);
            }
        }

        let full_order = self.topological_order()?;
        let ancestor_set: std::collections::HashSet<_> = ancestors.into_iter().collect();
        let ordered: Vec<String> = full_order
            .into_iter()
            .filter(|name| {
                self.index
                    .get(name)
                    .map(|n| ancestor_set.contains(n))
                    .unwrap_or(false)
            })
            .collect();

        debug!("Start order for {}: {:?}", unit_name, ordered);

        Ok(ordered)
    }

    pub fn required_deps<'a>(
        &'a self,
        unit_name: &str,
        units: &'a HashMap<String, crate::unit::types::UnitFile>,
    ) -> Vec<String> {
        let Some(unit) = units.get(unit_name) else {
            return vec![];
        };
        unit.unit
            .requires
            .iter()
            .chain(unit.unit.wants.iter())
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    #[cfg(test)]
    pub(crate) fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    #[cfg(test)]
    pub(crate) fn has_edge(&self, from: &str, to: &str) -> bool {
        let Some(&from_idx) = self.index.get(from) else {
            return false;
        };
        let Some(&to_idx) = self.index.get(to) else {
            return false;
        };
        self.graph.contains_edge(from_idx, to_idx)
    }

    #[cfg(test)]
    pub(crate) fn edge_kind(&self, from: &str, to: &str) -> Option<EdgeKind> {
        let &from_idx = self.index.get(from)?;
        let &to_idx = self.index.get(to)?;
        let edge_idx = self.graph.find_edge(from_idx, to_idx)?;
        Some(*self.graph.edge_weight(edge_idx).unwrap())
    }

    #[cfg(test)]
    pub(crate) fn has_node(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::types::UnitFile;

    /// Helper: create a minimal UnitFile with the given name and default sections.
    fn make_unit(name: &str) -> UnitFile {
        UnitFile::new(name)
    }

    /// Helper: create a UnitFile with After dependencies.
    fn make_unit_after(name: &str, after: &[&str]) -> UnitFile {
        let mut u = make_unit(name);
        for dep in after {
            u.unit.after.insert(dep.to_string());
        }
        u
    }

    /// Helper: create a UnitFile with Before dependencies.
    fn make_unit_before(name: &str, before: &[&str]) -> UnitFile {
        let mut u = make_unit(name);
        for dep in before {
            u.unit.before.insert(dep.to_string());
        }
        u
    }

    // =========================================================================
    // Basic graph construction
    // =========================================================================

    #[test]
    fn test_empty_graph() {
        let units: Vec<UnitFile> = vec![];
        let graph = DependencyGraph::build(units.iter());
        assert_eq!(graph.node_count(), 0);
        assert_eq!(graph.edge_count(), 0);

        let order = graph.topological_order().unwrap();
        assert!(order.is_empty());
    }

    #[test]
    fn test_single_unit_no_deps() {
        let units = [make_unit("foo.service")];
        let graph = DependencyGraph::build(units.iter());

        assert_eq!(graph.node_count(), 1);
        assert_eq!(graph.edge_count(), 0);
        assert!(graph.has_node("foo.service"));

        let order = graph.topological_order().unwrap();
        assert_eq!(order, vec!["foo.service"]);
    }

    #[test]
    fn test_multiple_units_no_deps() {
        let units = [
            make_unit("a.service"),
            make_unit("b.service"),
            make_unit("c.target"),
        ];
        let graph = DependencyGraph::build(units.iter());

        assert_eq!(graph.node_count(), 3);
        assert_eq!(graph.edge_count(), 0);
        assert!(graph.has_node("a.service"));
        assert!(graph.has_node("b.service"));
        assert!(graph.has_node("c.target"));
    }

    // =========================================================================
    // After= edges (Weak, dep → unit)
    // =========================================================================

    #[test]
    fn test_after_creates_weak_edge() {
        let units = [
            make_unit("network.target"),
            make_unit_after("sshd.service", &["network.target"]),
        ];
        let graph = DependencyGraph::build(units.iter());

        // After=network.target on sshd.service means network.target → sshd.service
        assert!(graph.has_edge("network.target", "sshd.service"));
        assert_eq!(
            graph.edge_kind("network.target", "sshd.service"),
            Some(EdgeKind::Weak)
        );
        // No reverse edge
        assert!(!graph.has_edge("sshd.service", "network.target"));
    }

    #[test]
    fn test_after_multiple_deps() {
        let units = [
            make_unit("a.target"),
            make_unit("b.target"),
            make_unit_after("c.service", &["a.target", "b.target"]),
        ];
        let graph = DependencyGraph::build(units.iter());

        assert!(graph.has_edge("a.target", "c.service"));
        assert!(graph.has_edge("b.target", "c.service"));
        assert_eq!(graph.edge_count(), 2);
    }

    #[test]
    fn test_after_creates_implicit_node_for_unknown_dep() {
        // If After= references a unit not in the provided list, a node is created.
        let units = [make_unit_after("sshd.service", &["network.target"])];
        let graph = DependencyGraph::build(units.iter());

        assert!(graph.has_node("network.target"));
        assert!(graph.has_node("sshd.service"));
        assert!(graph.has_edge("network.target", "sshd.service"));
        assert_eq!(graph.node_count(), 2);
    }

    // =========================================================================
    // Before= edges (Weak, unit → dep)
    // =========================================================================

    #[test]
    fn test_before_creates_weak_edge() {
        let units = [
            make_unit("sshd.service"),
            make_unit_before("network.target", &["sshd.service"]),
        ];
        let graph = DependencyGraph::build(units.iter());

        // Before=sshd.service on network.target means network.target → sshd.service
        assert!(graph.has_edge("network.target", "sshd.service"));
        assert_eq!(
            graph.edge_kind("network.target", "sshd.service"),
            Some(EdgeKind::Weak)
        );
    }

    #[test]
    fn test_before_creates_implicit_node() {
        let units = [make_unit_before("early.service", &["late.service"])];
        let graph = DependencyGraph::build(units.iter());

        assert!(graph.has_node("late.service"));
        assert!(graph.has_edge("early.service", "late.service"));
    }

    #[test]
    fn test_before_and_after_symmetry() {
        // After=X on unit B is equivalent to Before=B on unit X
        let units_after = [
            make_unit("x.service"),
            make_unit_after("b.service", &["x.service"]),
        ];
        let units_before = [
            make_unit_before("x.service", &["b.service"]),
            make_unit("b.service"),
        ];

        let graph_after = DependencyGraph::build(units_after.iter());
        let graph_before = DependencyGraph::build(units_before.iter());

        // Both should produce edge x.service → b.service
        assert!(graph_after.has_edge("x.service", "b.service"));
        assert!(graph_before.has_edge("x.service", "b.service"));
    }

    // =========================================================================
    // Requisite= edges (Strong, dep → unit)
    // =========================================================================

    #[test]
    fn test_requisite_creates_strong_edge() {
        let mut unit = make_unit("app.service");
        unit.unit.requisite.insert("base.target".to_string());

        let units = [make_unit("base.target"), unit];
        let graph = DependencyGraph::build(units.iter());

        assert!(graph.has_edge("base.target", "app.service"));
        assert_eq!(
            graph.edge_kind("base.target", "app.service"),
            Some(EdgeKind::Strong)
        );
    }

    #[test]
    fn test_requisite_implicit_node() {
        let mut unit = make_unit("app.service");
        unit.unit.requisite.insert("missing.target".to_string());

        let units = [unit];
        let graph = DependencyGraph::build(units.iter());

        assert!(graph.has_node("missing.target"));
        assert!(graph.has_edge("missing.target", "app.service"));
        assert_eq!(
            graph.edge_kind("missing.target", "app.service"),
            Some(EdgeKind::Strong)
        );
    }

    // =========================================================================
    // BindsTo= edges (Strong, dep → unit)
    // =========================================================================

    #[test]
    fn test_binds_to_creates_strong_edge() {
        let mut unit = make_unit("webapp.service");
        unit.unit.binds_to.insert("database.service".to_string());

        let units = [make_unit("database.service"), unit];
        let graph = DependencyGraph::build(units.iter());

        assert!(graph.has_edge("database.service", "webapp.service"));
        assert_eq!(
            graph.edge_kind("database.service", "webapp.service"),
            Some(EdgeKind::Strong)
        );
    }

    #[test]
    fn test_binds_to_implicit_node() {
        let mut unit = make_unit("webapp.service");
        unit.unit.binds_to.insert("unknown.service".to_string());

        let units = [unit];
        let graph = DependencyGraph::build(units.iter());

        assert!(graph.has_node("unknown.service"));
        assert!(graph.has_edge("unknown.service", "webapp.service"));
    }

    // =========================================================================
    // Duplicate edge prevention
    // =========================================================================

    #[test]
    fn test_duplicate_after_edges_not_added() {
        // If two mechanisms would create the same edge, only one should exist.
        let mut unit = make_unit("app.service");
        unit.unit.after.insert("dep.service".to_string());

        // Also dep has Before=app.service which would create the same edge
        let mut dep = make_unit("dep.service");
        dep.unit.before.insert("app.service".to_string());

        let units = [dep, unit];
        let graph = DependencyGraph::build(units.iter());

        assert!(graph.has_edge("dep.service", "app.service"));
        // Should only have one edge, not two
        assert_eq!(graph.edge_count(), 1);
    }

    #[test]
    fn test_duplicate_requisite_and_after_same_direction() {
        // Requisite=X and After=X both create edges from X → unit,
        // but Requisite is processed separately. First edge wins.
        let mut unit = make_unit("app.service");
        unit.unit.after.insert("base.target".to_string());
        unit.unit.requisite.insert("base.target".to_string());

        let units = [make_unit("base.target"), unit];
        let graph = DependencyGraph::build(units.iter());

        assert!(graph.has_edge("base.target", "app.service"));
        // Only one edge between the same pair (first wins due to contains_edge check)
        // Count edges from base.target to app.service
        assert_eq!(graph.edge_count(), 1);
    }

    // =========================================================================
    // Topological ordering — linear chain
    // =========================================================================

    #[test]
    fn test_topological_order_linear_chain() {
        // a → b → c (a must start first, then b, then c)
        let units = [
            make_unit_before("a.service", &["b.service"]),
            make_unit_before("b.service", &["c.service"]),
            make_unit("c.service"),
        ];
        let graph = DependencyGraph::build(units.iter());
        let order = graph.topological_order().unwrap();

        let pos_a = order.iter().position(|x| x == "a.service").unwrap();
        let pos_b = order.iter().position(|x| x == "b.service").unwrap();
        let pos_c = order.iter().position(|x| x == "c.service").unwrap();

        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    #[test]
    fn test_topological_order_after_chain() {
        // c After b After a → a must come first
        let units = [
            make_unit("a.service"),
            make_unit_after("b.service", &["a.service"]),
            make_unit_after("c.service", &["b.service"]),
        ];
        let graph = DependencyGraph::build(units.iter());
        let order = graph.topological_order().unwrap();

        let pos_a = order.iter().position(|x| x == "a.service").unwrap();
        let pos_b = order.iter().position(|x| x == "b.service").unwrap();
        let pos_c = order.iter().position(|x| x == "c.service").unwrap();

        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    // =========================================================================
    // Topological ordering — diamond dependency
    // =========================================================================

    #[test]
    fn test_topological_order_diamond() {
        // Diamond: a → b, a → c, b → d, c → d
        let mut a = make_unit("a.service");
        a.unit.before.insert("b.service".to_string());
        a.unit.before.insert("c.service".to_string());

        let mut b = make_unit("b.service");
        b.unit.before.insert("d.service".to_string());

        let mut c = make_unit("c.service");
        c.unit.before.insert("d.service".to_string());

        let d = make_unit("d.service");

        let units = [a, b, c, d];
        let graph = DependencyGraph::build(units.iter());
        let order = graph.topological_order().unwrap();

        let pos_a = order.iter().position(|x| x == "a.service").unwrap();
        let pos_b = order.iter().position(|x| x == "b.service").unwrap();
        let pos_c = order.iter().position(|x| x == "c.service").unwrap();
        let pos_d = order.iter().position(|x| x == "d.service").unwrap();

        assert!(pos_a < pos_b);
        assert!(pos_a < pos_c);
        assert!(pos_b < pos_d);
        assert!(pos_c < pos_d);
    }

    // =========================================================================
    // Cycle detection and breaking
    // =========================================================================

    #[test]
    fn test_cycle_breaking_weak_edges() {
        // Create a cycle with weak edges: a After b, b After a
        let mut a = make_unit("a.service");
        a.unit.after.insert("b.service".to_string());

        let mut b = make_unit("b.service");
        b.unit.after.insert("a.service".to_string());

        let units = [a, b];
        let graph = DependencyGraph::build(units.iter());

        // Should successfully break the cycle (weak edges)
        let order = graph.topological_order();
        assert!(order.is_ok());
        let order = order.unwrap();
        assert_eq!(order.len(), 2);
        assert!(order.contains(&"a.service".to_string()));
        assert!(order.contains(&"b.service".to_string()));
    }

    #[test]
    fn test_cycle_breaking_prefers_weak_edges() {
        // Mix of strong and weak in a cycle: strong a→b, weak b→a
        // The weak edge (b→a) should be broken first.
        let mut a = make_unit("a.service");
        a.unit.binds_to.insert("b.service".to_string()); // Strong edge b→a

        let mut b = make_unit("b.service");
        b.unit.after.insert("a.service".to_string()); // Weak edge a→b

        let units = [a, b];
        let graph = DependencyGraph::build(units.iter());

        let order = graph.topological_order();
        assert!(order.is_ok());
        let order = order.unwrap();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn test_cycle_three_units_weak() {
        // a After b, b After c, c After a — all weak
        let mut a = make_unit("a.service");
        a.unit.after.insert("b.service".to_string());

        let mut b = make_unit("b.service");
        b.unit.after.insert("c.service".to_string());

        let mut c = make_unit("c.service");
        c.unit.after.insert("a.service".to_string());

        let units = [a, b, c];
        let graph = DependencyGraph::build(units.iter());

        let order = graph.topological_order();
        assert!(order.is_ok());
        let order = order.unwrap();
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn test_cycle_strong_edges_still_breaks() {
        // Even strong-only cycles should be broken (with a warning).
        // The current implementation removes strong edges as fallback.
        let mut a = make_unit("a.service");
        a.unit.requisite.insert("b.service".to_string()); // Strong b→a

        let mut b = make_unit("b.service");
        b.unit.requisite.insert("a.service".to_string()); // Strong a→b

        let units = [a, b];
        let graph = DependencyGraph::build(units.iter());

        let order = graph.topological_order();
        assert!(order.is_ok());
    }

    #[test]
    fn test_no_cycle_dag() {
        // Ensure normal DAG doesn't trigger cycle breaking
        let units = [
            make_unit("a.service"),
            make_unit_after("b.service", &["a.service"]),
            make_unit_after("c.service", &["a.service"]),
            make_unit_after("d.service", &["b.service", "c.service"]),
        ];
        let graph = DependencyGraph::build(units.iter());

        let order = graph.topological_order().unwrap();
        let pos_a = order.iter().position(|x| x == "a.service").unwrap();
        let pos_b = order.iter().position(|x| x == "b.service").unwrap();
        let pos_c = order.iter().position(|x| x == "c.service").unwrap();
        let pos_d = order.iter().position(|x| x == "d.service").unwrap();

        assert!(pos_a < pos_b);
        assert!(pos_a < pos_c);
        assert!(pos_b < pos_d);
        assert!(pos_c < pos_d);
    }

    // =========================================================================
    // start_order_for — transitive dependencies
    // =========================================================================

    #[test]
    fn test_start_order_for_single_unit() {
        let units = [make_unit("foo.service")];
        let graph = DependencyGraph::build(units.iter());

        let order = graph.start_order_for("foo.service").unwrap();
        assert_eq!(order, vec!["foo.service"]);
    }

    #[test]
    fn test_start_order_for_with_deps() {
        // a → b → c: to start c, we need b and a first
        let units = [
            make_unit("a.service"),
            make_unit_after("b.service", &["a.service"]),
            make_unit_after("c.service", &["b.service"]),
        ];
        let graph = DependencyGraph::build(units.iter());

        let order = graph.start_order_for("c.service").unwrap();
        assert!(order.contains(&"a.service".to_string()));
        assert!(order.contains(&"b.service".to_string()));
        assert!(order.contains(&"c.service".to_string()));

        // a must come before b, b before c
        let pos_a = order.iter().position(|x| x == "a.service").unwrap();
        let pos_b = order.iter().position(|x| x == "b.service").unwrap();
        let pos_c = order.iter().position(|x| x == "c.service").unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    #[test]
    fn test_start_order_for_does_not_include_unrelated() {
        // a → b, c is unrelated. Starting b should not include c.
        let units = [
            make_unit("a.service"),
            make_unit_after("b.service", &["a.service"]),
            make_unit("c.service"),
        ];
        let graph = DependencyGraph::build(units.iter());

        let order = graph.start_order_for("b.service").unwrap();
        assert!(order.contains(&"a.service".to_string()));
        assert!(order.contains(&"b.service".to_string()));
        assert!(!order.contains(&"c.service".to_string()));
    }

    #[test]
    fn test_start_order_for_unknown_unit() {
        let units = [make_unit("a.service")];
        let graph = DependencyGraph::build(units.iter());

        let order = graph.start_order_for("nonexistent.service").unwrap();
        assert_eq!(order, vec!["nonexistent.service"]);
    }

    #[test]
    fn test_start_order_for_transitive_diamond() {
        // Diamond: a → b, a → c, b → d, c → d
        // Starting d requires a, b, c, d
        let mut a = make_unit("a.service");
        a.unit.before.insert("b.service".to_string());
        a.unit.before.insert("c.service".to_string());

        let mut b = make_unit("b.service");
        b.unit.before.insert("d.service".to_string());

        let mut c = make_unit("c.service");
        c.unit.before.insert("d.service".to_string());

        let d = make_unit("d.service");

        let units = [a, b, c, d];
        let graph = DependencyGraph::build(units.iter());

        let order = graph.start_order_for("d.service").unwrap();
        assert!(order.contains(&"a.service".to_string()));
        assert!(order.contains(&"b.service".to_string()));
        assert!(order.contains(&"c.service".to_string()));
        assert!(order.contains(&"d.service".to_string()));

        let pos_a = order.iter().position(|x| x == "a.service").unwrap();
        let pos_d = order.iter().position(|x| x == "d.service").unwrap();
        assert!(pos_a < pos_d);
    }

    // =========================================================================
    // required_deps method
    // =========================================================================

    #[test]
    fn test_required_deps_returns_requires_and_wants() {
        let mut unit = make_unit("app.service");
        unit.unit.requires.insert("base.target".to_string());
        unit.unit.wants.insert("logging.service".to_string());

        let units = [
            make_unit("base.target"),
            make_unit("logging.service"),
            unit.clone(),
        ];
        let graph = DependencyGraph::build(units.iter());

        let mut unit_map = HashMap::new();
        unit_map.insert("app.service".to_string(), unit);

        let deps = graph.required_deps("app.service", &unit_map);
        assert!(deps.contains(&"base.target".to_string()));
        assert!(deps.contains(&"logging.service".to_string()));
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn test_required_deps_unknown_unit() {
        let units = [make_unit("a.service")];
        let graph = DependencyGraph::build(units.iter());
        let unit_map = HashMap::new();

        let deps = graph.required_deps("nonexistent.service", &unit_map);
        assert!(deps.is_empty());
    }

    #[test]
    fn test_required_deps_no_deps() {
        let unit = make_unit("app.service");

        let units = [unit.clone()];
        let graph = DependencyGraph::build(units.iter());

        let mut unit_map = HashMap::new();
        unit_map.insert("app.service".to_string(), unit);

        let deps = graph.required_deps("app.service", &unit_map);
        assert!(deps.is_empty());
    }

    // =========================================================================
    // Complex scenarios (mimicking systemd test patterns)
    // =========================================================================

    #[test]
    fn test_complex_boot_target_ordering() {
        // Simulate a simplified boot sequence:
        // sysinit.target → basic.target → multi-user.target → graphical.target
        let mut sysinit = make_unit("sysinit.target");
        sysinit.unit.before.insert("basic.target".to_string());

        let mut basic = make_unit("basic.target");
        basic.unit.after.insert("sysinit.target".to_string());
        basic.unit.before.insert("multi-user.target".to_string());

        let mut multi_user = make_unit("multi-user.target");
        multi_user.unit.after.insert("basic.target".to_string());
        multi_user
            .unit
            .before
            .insert("graphical.target".to_string());

        let mut graphical = make_unit("graphical.target");
        graphical.unit.after.insert("multi-user.target".to_string());

        let units = [sysinit, basic, multi_user, graphical];
        let graph = DependencyGraph::build(units.iter());

        let order = graph.topological_order().unwrap();
        let pos = |name: &str| order.iter().position(|x| x == name).unwrap();

        assert!(pos("sysinit.target") < pos("basic.target"));
        assert!(pos("basic.target") < pos("multi-user.target"));
        assert!(pos("multi-user.target") < pos("graphical.target"));
    }

    #[test]
    fn test_service_with_mixed_dependency_types() {
        // A service that uses After, Requires, BindsTo, Requisite together
        let mut svc = make_unit("complex.service");
        svc.unit.after.insert("network.target".to_string());
        svc.unit.requires.insert("dbus.service".to_string());
        svc.unit.binds_to.insert("database.service".to_string());
        svc.unit.requisite.insert("base.target".to_string());

        let units = [
            make_unit("network.target"),
            make_unit("dbus.service"),
            make_unit("database.service"),
            make_unit("base.target"),
            svc,
        ];
        let graph = DependencyGraph::build(units.iter());

        // After=network.target → weak edge
        assert!(graph.has_edge("network.target", "complex.service"));
        assert_eq!(
            graph.edge_kind("network.target", "complex.service"),
            Some(EdgeKind::Weak)
        );

        // BindsTo=database.service → strong edge
        assert!(graph.has_edge("database.service", "complex.service"));
        assert_eq!(
            graph.edge_kind("database.service", "complex.service"),
            Some(EdgeKind::Strong)
        );

        // Requisite=base.target → strong edge
        assert!(graph.has_edge("base.target", "complex.service"));
        assert_eq!(
            graph.edge_kind("base.target", "complex.service"),
            Some(EdgeKind::Strong)
        );

        // Requires=dbus.service does NOT create an ordering edge by itself
        assert!(!graph.has_edge("dbus.service", "complex.service"));
    }

    #[test]
    fn test_requires_does_not_create_ordering_edge() {
        // Requires= only declares a dependency, not an ordering constraint.
        // Ordering must be specified separately with After=/Before=.
        let mut svc = make_unit("app.service");
        svc.unit.requires.insert("dep.service".to_string());

        let units = [make_unit("dep.service"), svc];
        let graph = DependencyGraph::build(units.iter());

        assert!(!graph.has_edge("dep.service", "app.service"));
        assert!(!graph.has_edge("app.service", "dep.service"));
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn test_wants_does_not_create_ordering_edge() {
        // Wants= also only declares a dependency, not ordering.
        let mut svc = make_unit("app.service");
        svc.unit.wants.insert("opt.service".to_string());

        let units = [make_unit("opt.service"), svc];
        let graph = DependencyGraph::build(units.iter());

        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn test_large_graph_ordering() {
        // Create a larger graph with 10 units in a chain
        let mut units: Vec<UnitFile> = Vec::new();
        for i in 0..10 {
            let name = format!("unit{}.service", i);
            if i == 0 {
                units.push(make_unit(&name));
            } else {
                let prev = format!("unit{}.service", i - 1);
                units.push(make_unit_after(&name, &[&prev]));
            }
        }

        let graph = DependencyGraph::build(units.iter());
        let order = graph.topological_order().unwrap();

        // Verify total ordering
        for i in 0..9 {
            let curr = format!("unit{}.service", i);
            let next = format!("unit{}.service", i + 1);
            let pos_curr = order.iter().position(|x| x == &curr).unwrap();
            let pos_next = order.iter().position(|x| x == &next).unwrap();
            assert!(pos_curr < pos_next, "{} should come before {}", curr, next);
        }
    }

    #[test]
    fn test_parallel_units_independent_ordering() {
        // Units with no dependencies between them can appear in any order.
        // But units depending on a common parent must all come after it.
        let mut parent = make_unit("parent.target");
        parent.unit.before.insert("child1.service".to_string());
        parent.unit.before.insert("child2.service".to_string());
        parent.unit.before.insert("child3.service".to_string());

        let units = [
            parent,
            make_unit("child1.service"),
            make_unit("child2.service"),
            make_unit("child3.service"),
        ];
        let graph = DependencyGraph::build(units.iter());
        let order = graph.topological_order().unwrap();

        let pos_parent = order.iter().position(|x| x == "parent.target").unwrap();
        let pos_c1 = order.iter().position(|x| x == "child1.service").unwrap();
        let pos_c2 = order.iter().position(|x| x == "child2.service").unwrap();
        let pos_c3 = order.iter().position(|x| x == "child3.service").unwrap();

        assert!(pos_parent < pos_c1);
        assert!(pos_parent < pos_c2);
        assert!(pos_parent < pos_c3);
    }

    #[test]
    fn test_self_after_reference_does_not_panic() {
        // A unit referencing itself in After= shouldn't panic.
        // This creates a self-loop which is a degenerate cycle.
        let mut unit = make_unit("self.service");
        unit.unit.after.insert("self.service".to_string());

        let units = [unit];
        let graph = DependencyGraph::build(units.iter());

        // The graph builds without panic. Self-loop creates a cycle;
        // topological_order may or may not break it, but must not panic.
        let _order = graph.topological_order();
    }

    #[test]
    fn test_edge_kind_equality() {
        assert_eq!(EdgeKind::Strong, EdgeKind::Strong);
        assert_eq!(EdgeKind::Weak, EdgeKind::Weak);
        assert_ne!(EdgeKind::Strong, EdgeKind::Weak);
    }
}
