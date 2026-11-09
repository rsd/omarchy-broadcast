//! The consumer count: the Link globals of the registry whose output node
//! is ours. Pure bookkeeping, fed by the registry listener.
//!
//! A consumer opening the node creates a Link (`link.output.node` = our
//! node id) and its removal removes it, also when the consumer dies
//! (research, spike result 3). Removals carry no type, so the table keeps
//! every Link it was told about and ignores other ids. Our node id is known
//! only once the stream is registered, and changes on every reconnect:
//! links are kept whatever their output node, and counted against the
//! current id. Node `running` is not used: it means linked, not producing.
//!
//! **A link in error is not a consumer.** A consumer whose negotiation
//! failed can keep its stream, and so its Link global, alive: counting it
//! would keep the camera on with no frame delivered. The stream binds each
//! Link from our node ([`LinkTable::ours`]) and follows its state; a link
//! in state `Error` stays in the table, flagged ([`LinkTable::set_failed`]),
//! and is not counted until its state leaves `Error`. A link whose state is
//! not known yet (just announced, being negotiated) counts.

use std::collections::HashMap;

/// The `link.output.node` and `link.input.node` of a Link global, if both
/// are present and numeric.
pub fn link_nodes(output: Option<&str>, input: Option<&str>) -> Option<(u32, u32)> {
    Some((output?.trim().parse().ok()?, input?.trim().parse().ok()?))
}

/// One Link global.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    output: u32,
    input: u32,
    /// Its state is `Error`.
    failed: bool,
}

/// The registry's links and our node id.
#[derive(Debug, Default, Clone)]
pub struct LinkTable {
    links: HashMap<u32, Entry>,
    node: Option<u32>,
}

impl LinkTable {
    /// A Link global appeared (or was announced again: idempotent, the
    /// known state of the same link is kept).
    pub fn add(&mut self, link: u32, output_node: u32, input_node: u32) {
        let failed = self
            .links
            .get(&link)
            .is_some_and(|e| e.output == output_node && e.input == input_node && e.failed);
        self.links.insert(
            link,
            Entry {
                output: output_node,
                input: input_node,
                failed,
            },
        );
    }

    /// The link's state as its info reports it: `Error` (`true`) or any
    /// other (`false`). A no-op for an unknown id.
    pub fn set_failed(&mut self, link: u32, failed: bool) {
        if let Some(e) = self.links.get_mut(&link) {
            e.failed = failed;
        }
    }

    /// Whether the known link `link` is in error (`None`: unknown).
    pub fn is_failed(&self, link: u32) -> Option<bool> {
        self.links.get(&link).map(|e| e.failed)
    }

    /// The links from our node, whatever their state: the ones to follow.
    /// None while the node is unknown.
    pub fn ours(&self) -> Vec<u32> {
        let Some(node) = self.node else {
            return Vec::new();
        };
        let mut ids: Vec<u32> = self
            .links
            .iter()
            .filter(|(_, e)| e.output == node)
            .map(|(&id, _)| id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// A global was removed: a no-op unless it is a known link.
    pub fn remove(&mut self, id: u32) {
        self.links.remove(&id);
    }

    /// Our node id: `None` while the stream is not registered.
    pub fn set_node(&mut self, node: Option<u32>) {
        self.node = node;
    }

    pub fn node(&self) -> Option<u32> {
        self.node
    }

    /// Forgets every link and the node (the connection is gone).
    pub fn clear(&mut self) {
        self.links.clear();
        self.node = None;
    }

    /// Links from our node not in error: the consumer count. 0 while the
    /// node is unknown.
    pub fn consumers(&self) -> u32 {
        let Some(node) = self.node else {
            return 0;
        };
        let n = self
            .links
            .values()
            .filter(|e| e.output == node && !e.failed)
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_props_parse() {
        assert_eq!(link_nodes(Some("42"), Some("7")), Some((42, 7)));
        assert_eq!(link_nodes(Some("42"), None), None);
        assert_eq!(link_nodes(None, Some("7")), None);
        assert_eq!(link_nodes(Some("x"), Some("7")), None);
    }

    #[test]
    fn links_from_our_node_are_counted() {
        let mut t = LinkTable::default();
        t.set_node(Some(50));
        assert_eq!(t.consumers(), 0);
        t.add(100, 50, 60);
        t.add(101, 50, 61);
        // Another node's link, and a link into our node: not consumers.
        t.add(102, 70, 71);
        t.add(103, 72, 50);
        assert_eq!(t.consumers(), 2);
        // A duplicate announcement counts once.
        t.add(100, 50, 60);
        assert_eq!(t.consumers(), 2);
        t.remove(100);
        assert_eq!(t.consumers(), 1);
        // An unknown id (a port, a node) and a second removal: no-ops.
        t.remove(999);
        t.remove(100);
        assert_eq!(t.consumers(), 1);
        t.remove(102);
        assert_eq!(t.consumers(), 1);
        t.remove(101);
        assert_eq!(t.consumers(), 0);
    }

    /// Links seen before our node id is known count once it is.
    #[test]
    fn links_seen_before_the_node_id_count_once_it_is_known() {
        let mut t = LinkTable::default();
        t.add(100, 50, 60);
        t.add(101, 51, 61);
        assert_eq!(t.consumers(), 0);
        t.set_node(Some(50));
        assert_eq!(t.consumers(), 1);
        t.set_node(None);
        assert_eq!(t.consumers(), 0);
    }

    /// After a reconnect our node has a new id: links of the old one no
    /// longer count, those of the new one do.
    #[test]
    fn the_count_follows_the_node_id_across_a_reconnect() {
        let mut t = LinkTable::default();
        t.set_node(Some(50));
        t.add(100, 50, 60);
        assert_eq!(t.consumers(), 1);
        t.clear();
        assert_eq!((t.consumers(), t.node()), (0, None));
        // The old link announced again by a fresh registry, then the new
        // node.
        t.add(100, 50, 60);
        t.set_node(Some(80));
        assert_eq!(t.consumers(), 0);
        t.add(200, 80, 90);
        assert_eq!(t.consumers(), 1);
        t.set_node(Some(50));
        assert_eq!(t.consumers(), 1);
    }

    /// A link in error stays known but is not counted; it counts again
    /// once its state leaves `Error`; removing it is as for any link.
    #[test]
    fn a_link_in_error_is_not_counted_until_it_recovers() {
        let mut t = LinkTable::default();
        t.set_node(Some(50));
        t.add(100, 50, 60);
        t.add(101, 50, 61);
        assert_eq!((t.consumers(), t.ours()), (2, vec![100, 101]));
        t.set_failed(100, true);
        assert_eq!(t.consumers(), 1);
        assert_eq!(t.is_failed(100), Some(true));
        // Still followed.
        assert_eq!(t.ours(), vec![100, 101]);
        // A re-announcement keeps the known state.
        t.add(100, 50, 60);
        assert_eq!(t.consumers(), 1);
        // Recovered.
        t.set_failed(100, false);
        assert_eq!(t.consumers(), 2);
        // Both in error: nobody consumes.
        t.set_failed(100, true);
        t.set_failed(101, true);
        assert_eq!(t.consumers(), 0);
        // An unknown id: a no-op.
        t.set_failed(999, true);
        assert_eq!(t.is_failed(999), None);
        // Removing an errored link forgets it.
        t.remove(100);
        assert_eq!((t.is_failed(100), t.ours()), (None, vec![101]));
        t.set_failed(101, false);
        assert_eq!(t.consumers(), 1);
        t.remove(101);
        assert_eq!((t.consumers(), t.ours()), (0, vec![]));
    }

    /// Only links from our node are followed, and only once it is known;
    /// a reconnect forgets every state.
    #[test]
    fn ours_follows_the_node_id() {
        let mut t = LinkTable::default();
        t.add(100, 50, 60);
        t.add(102, 70, 71);
        t.add(103, 72, 50);
        assert!(t.ours().is_empty());
        t.set_node(Some(50));
        assert_eq!(t.ours(), vec![100]);
        t.set_failed(100, true);
        t.clear();
        t.add(100, 50, 60);
        t.set_node(Some(50));
        assert_eq!((t.is_failed(100), t.consumers()), (Some(false), 1));
        // The same id announced for other nodes: a new link, state unknown.
        t.set_failed(100, true);
        t.add(100, 50, 61);
        assert_eq!(t.consumers(), 1);
    }
}
