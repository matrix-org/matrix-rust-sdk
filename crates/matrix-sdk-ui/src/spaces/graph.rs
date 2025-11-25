// Copyright 2025 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for that specific language governing permissions and
// limitations under the License.

use std::collections::{BTreeMap, BTreeSet};

use ruma::{OwnedRoomId, RoomId};

#[derive(Debug)]
struct SpaceGraphNode {
    id: OwnedRoomId,
    parents: BTreeSet<OwnedRoomId>,
    children: BTreeSet<OwnedRoomId>,
}

impl SpaceGraphNode {
    fn new(id: OwnedRoomId) -> Self {
        Self { id, parents: BTreeSet::new(), children: BTreeSet::new() }
    }
}

/// A graph structure representing a space hierarchy. Contains functionality
/// for mapping parent-child relationships between rooms, removing cycles and
/// retrieving top-level parents/roots.
#[derive(Debug)]
pub(super) struct SpaceGraph {
    nodes: BTreeMap<OwnedRoomId, SpaceGraphNode>,
}

impl Default for SpaceGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl SpaceGraph {
    /// Creates a new empty space graph containing no nodes or edges.
    pub(super) fn new() -> Self {
        Self { nodes: BTreeMap::new() }
    }

    /// Returns the root nodes of the graph, which are nodes without any
    /// parents.
    pub(super) fn root_nodes(&self) -> Vec<&RoomId> {
        self.nodes
            .values()
            .filter(|node| node.parents.is_empty())
            .map(|node| node.id.as_ref())
            .collect()
    }

    /// Returns the children of a given node. If the node does not exist, it
    /// returns an empty vector.
    pub(super) fn children_of(&self, node_id: &RoomId) -> Vec<&RoomId> {
        self.nodes
            .get(node_id)
            .map_or(vec![], |node| node.children.iter().map(|id| id.as_ref()).collect())
    }

    /// Returns the parents of a given node. If the node does not exist, it
    /// returns an empty vector.
    pub(super) fn parents_of(&self, node_id: &RoomId) -> Vec<&RoomId> {
        self.nodes
            .get(node_id)
            .map_or(vec![], |node| node.parents.iter().map(|id| id.as_ref()).collect())
    }

    /// Adds a node to the graph. If the node already exists, it does nothing.
    pub(super) fn add_node(&mut self, node_id: OwnedRoomId) {
        self.nodes.entry(node_id.clone()).or_insert(SpaceGraphNode::new(node_id));
    }

    /// Returns whether a node exists in the graph.
    pub(super) fn has_node(&self, node_id: &RoomId) -> bool {
        self.nodes.contains_key(node_id)
    }

    /// Adds a directed edge from `parent_id` to `child_id`, creating nodes if
    /// they do not already exist in the graph.
    pub(super) fn add_edge(&mut self, parent_id: OwnedRoomId, child_id: OwnedRoomId) {
        let parent_entry =
            self.nodes.entry(parent_id.clone()).or_insert(SpaceGraphNode::new(parent_id.clone()));
        parent_entry.children.insert(child_id.clone());

        let child_entry =
            self.nodes.entry(child_id.clone()).or_insert(SpaceGraphNode::new(child_id));
        child_entry.parents.insert(parent_id);
    }

    /// Returns the subtree of the given node in a bottom-up order.
    ///
    /// Does a BFS starting from the given node tracking the visited nodes
    /// and returning them in the reverse order.
    pub(super) fn flattened_bottom_up_subtree(&self, node_id: &RoomId) -> Vec<OwnedRoomId> {
        if !self.has_node(node_id) {
            return Vec::new();
        }

        let mut stack = vec![node_id.to_owned()];
        let mut result = Vec::new();

        while let Some(node) = stack.pop() {
            result.insert(0, node.clone());

            if let Some(node) = self.nodes.get(&node) {
                for child in &node.children {
                    stack.push(child.to_owned());
                }
            }
        }

        result
    }

    /// Removes cycles in the graph by performing a depth-first search (DFS) and
    /// remembering the visited nodes. If a node is revisited while still in the
    /// current path (i.e. it's on the stack), it indicates a cycle.
    pub(super) fn remove_cycles(&mut self) {
        let mut visited = BTreeSet::new();
        let mut stack = BTreeSet::new();

        let mut edges_to_remove = Vec::new();

        for node_id in self.nodes.keys() {
            self.dfs_remove_cycles(node_id, &mut visited, &mut stack, &mut edges_to_remove);
        }

        for (parent, child) in edges_to_remove {
            if let Some(node) = self.nodes.get_mut(&parent) {
                node.children.remove(&child);
            }
            if let Some(node) = self.nodes.get_mut(&child) {
                node.parents.remove(&parent);
            }
        }
    }

    fn dfs_remove_cycles(
        &self,
        node_id: &OwnedRoomId,
        visited: &mut BTreeSet<OwnedRoomId>,
        stack: &mut BTreeSet<OwnedRoomId>,
        edges_to_remove: &mut Vec<(OwnedRoomId, OwnedRoomId)>,
    ) {
        if !visited.insert(node_id.clone()) {
            return;
        }

        stack.insert(node_id.clone());

        if let Some(node) = self.nodes.get(node_id) {
            for child in &node.children {
                if stack.contains(child) {
                    // Found a cycle → mark this edge for removal
                    edges_to_remove.push((node_id.clone(), child.clone()));
                } else {
                    self.dfs_remove_cycles(child, visited, stack, edges_to_remove);
                }
            }
        }

        stack.remove(node_id);
    }
}

#[cfg(test)]
mod tests {
    use ruma::{owned_room_id, room_id};

    use super::*;

    #[test]
    fn test_add_edge_and_root_nodes() {
        let mut graph = SpaceGraph::new();

        let a = room_id!("!a:example.org").to_owned();
        let b = room_id!("!b:example.org").to_owned();
        let c = room_id!("!c:example.org").to_owned();

        graph.add_edge(a.clone(), b.clone());
        graph.add_edge(a.clone(), c.clone());

        assert_eq!(graph.root_nodes(), vec![&a]);

        assert_eq!(graph.parents_of(&b), vec![&a]);
        assert_eq!(graph.parents_of(&c), vec![&a]);

        assert_eq!(graph.children_of(&a), vec![&b, &c]);
    }

    #[test]
    fn test_remove_cycles() {
        let mut graph = SpaceGraph::new();

        let a = room_id!("!a:example.org").to_owned();
        let b = room_id!("!b:example.org").to_owned();
        let c = room_id!("!c:example.org").to_owned();

        graph.add_edge(a.clone(), b.clone());
        graph.add_edge(b, c.clone());
        graph.add_edge(c.clone(), a.clone()); // creates a cycle

        assert!(graph.nodes[&c].children.contains(&a));

        graph.remove_cycles();

        assert!(!graph.nodes[&c].children.contains(&a));
        assert!(!graph.nodes[&a].parents.contains(&c));
    }

    #[test]
    fn test_disconnected_graph_roots() {
        let mut graph = SpaceGraph::new();

        let a = room_id!("!a:example.org").to_owned();
        let b = room_id!("!b:example.org").to_owned();
        graph.add_edge(a.clone(), b);

        let x = room_id!("!x:example.org").to_owned();
        let y = room_id!("!y:example.org").to_owned();
        graph.add_edge(x.clone(), y);

        let mut roots = graph.root_nodes();
        roots.sort_by_key(|key| key.to_string());

        let expected: Vec<&OwnedRoomId> = vec![&a, &x];
        assert_eq!(roots, expected);
    }

    #[test]
    fn test_multiple_parents() {
        let mut graph = SpaceGraph::new();

        let a = room_id!("!a:example.org").to_owned();
        let b = room_id!("!b:example.org").to_owned();
        let c = room_id!("!c:example.org").to_owned();
        let d = room_id!("!d:example.org").to_owned();

        graph.add_edge(a.clone(), c.clone());
        graph.add_edge(b.clone(), c.clone());
        graph.add_edge(c.clone(), d);

        let mut roots = graph.root_nodes();
        roots.sort_by_key(|key| key.to_string());

        let expected = vec![&a, &b];
        assert_eq!(roots, expected);

        let c_parents = &graph.nodes[&c].parents;
        assert!(c_parents.contains(&a));
        assert!(c_parents.contains(&b));
    }

    #[test]
    fn test_flattened_bottom_up_subtree() {
        let graph = vehicle_graph();

        let children = graph.flattened_bottom_up_subtree(room_id!("!personal:x.y"));
        let expected = vec![
            owned_room_id!("!gravel:x.y"),
            owned_room_id!("!mountain:x.y"),
            owned_room_id!("!road:x.y"),
            owned_room_id!("!bicycle:x.y"),
            owned_room_id!("!car:x.y"),
            owned_room_id!("!helicopter:x.y"),
            owned_room_id!("!personal:x.y"),
        ];

        assert_eq!(children, expected);

        let children = graph.flattened_bottom_up_subtree(room_id!("!shared:x.y"));
        let expected = vec![
            owned_room_id!("!bus:x.y"),
            owned_room_id!("!train:x.y"),
            owned_room_id!("!shared:x.y"),
        ];

        assert_eq!(children, expected);

        let children = graph.flattened_bottom_up_subtree(room_id!("!plane:x.y"));
        let expected = vec![owned_room_id!("!plane:x.y")];

        assert_eq!(children, expected);

        let children = graph.flattened_bottom_up_subtree(room_id!("!floo_powder:x.y"));
        assert!(children.is_empty());
    }

    fn vehicle_graph() -> SpaceGraph {
        // Vehicles
        // ├── Shared
        // │   ├── Bus
        // │   └── Train
        // ├── Personal
        // │   ├── Car
        // │   ├── Bicycle
        // │   │   ├── Road
        // │   │   ├── Gravel
        // │   │   └── Mountain
        // │   └── Helicopter
        // └── Cargo
        //     └── Plane

        let mut graph = SpaceGraph::new();

        graph.add_edge(owned_room_id!("!vehicles:x.y"), owned_room_id!("!shared:x.y"));
        graph.add_edge(owned_room_id!("!shared:x.y"), owned_room_id!("!bus:x.y"));
        graph.add_edge(owned_room_id!("!shared:x.y"), owned_room_id!("!train:x.y"));

        graph.add_edge(owned_room_id!("!vehicles:x.y"), owned_room_id!("!personal:x.y"));
        graph.add_edge(owned_room_id!("!personal:x.y"), owned_room_id!("!car:x.y"));

        graph.add_edge(owned_room_id!("!personal:x.y"), owned_room_id!("!bicycle:x.y"));
        graph.add_edge(owned_room_id!("!bicycle:x.y"), owned_room_id!("!road:x.y"));
        graph.add_edge(owned_room_id!("!bicycle:x.y"), owned_room_id!("!gravel:x.y"));
        graph.add_edge(owned_room_id!("!bicycle:x.y"), owned_room_id!("!mountain:x.y"));

        graph.add_edge(owned_room_id!("!personal:x.y"), owned_room_id!("!helicopter:x.y"));

        graph.add_edge(owned_room_id!("!vehicles:x.y"), owned_room_id!("!cargo:x.y"));
        graph.add_edge(owned_room_id!("!cargo:x.y"), owned_room_id!("!plane:x.y"));

        graph
    }
}
