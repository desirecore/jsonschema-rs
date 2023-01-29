use crate::{
    schema::{
        error::Result,
        resolving::{id_of_object, is_local, Reference, Resolver},
    },
    value_type::ValueType,
    vocabularies::{
        applicator::{AllOf, Properties},
        references::Ref,
        validation::{MaxLength, Maximum, MinProperties, Type},
        Keyword,
    },
};
use serde_json::{Map, Value};
use std::collections::{hash_map::Entry, HashMap, VecDeque};

use crate::{schema::error::Error, vocabularies::applicator::Items};
use std::ops::Range;
use url::Url;

/// A label on an edge between two JSON values.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub(crate) enum EdgeLabel {
    /// # Example
    ///
    /// `{"name": "Test"}` could be represented as:
    ///
    ///           name
    /// object ---------> "Test"
    ///
    /// The label for the edge between the top-level object and string "Test" is `name`.
    Key(Box<str>),
    /// # Example
    ///
    /// `["Test"]` could be represented as:
    ///
    ///          0
    /// array ------> "Test"
    ///
    /// The label for the edge between the top-level array and string "Test" is `0`.
    Index(usize),
}

impl EdgeLabel {
    pub(crate) fn as_key(&self) -> Option<&str> {
        if let EdgeLabel::Key(key) = self {
            Some(&**key)
        } else {
            None
        }
    }
}

impl From<usize> for EdgeLabel {
    fn from(value: usize) -> Self {
        EdgeLabel::Index(value)
    }
}

impl From<&String> for EdgeLabel {
    fn from(value: &String) -> Self {
        EdgeLabel::Key(value.to_owned().into_boxed_str())
    }
}

/// Unique identifier of a node in a graph.
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub(crate) struct NodeId(usize);

impl NodeId {
    pub(crate) fn value(&self) -> usize {
        self.0
    }
    /// If this `NodeId` points to the root node.
    pub(crate) fn is_root(&self) -> bool {
        self.value() == 0
    }
}

/// An edge between two JSON values stored in adjacency list.
///
/// # Example
///
/// JSON:
///
/// ```json
/// {
///     "properties": {
///         "A": {
///             "type": "object"
///         },
///         "B": {
///             "type": "string"
///         }
///     }
/// }
/// ```
///
/// ("A", 1) - an edge between `<properties>` and `<type: object>`
/// ("B", 2) - an edge between `<properties>` and `<type: string>`
///
/// ```text
///   Nodes                      Edges
///
/// [                         [
///   0 <properties>            [("A", 1), ("B", 2)]
///   1 <type: object>          []
///   2 <type: string>          []
/// ]                         ]
/// ```
#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub(crate) struct Edge {
    pub(crate) label: EdgeLabel,
    pub(crate) target: NodeId,
}

impl Edge {
    pub(crate) fn new(label: impl Into<EdgeLabel>, target: NodeId) -> Edge {
        Edge {
            label: label.into(),
            target,
        }
    }
}

/// An edge between a single JSON value and a range of JSON values that are stored contiguously.
///
/// # Example
///
/// JSON:
///
/// ```json
/// {
///     "properties": {
///         "A": {
///             "type": "object",
///             "maxLength": 5
///         },
///         "B": {
///             "type": "string"
///         }
///     }
/// }
/// ```
///
/// ("A", 1..3) - an edge between `<properties>` and `<type: object>` & `<maxLength: 5>`
/// ("B", 3..4) - an edge between `<properties>` and `<type: string>`
///
/// ```text
///   Nodes                                                              Edges
///
/// [                                                                 [
/// -- 0..1 `/`                                    |------------>     -- 0..2 (`properties' edges)
///      <properties> -----> 0..2 ---------------->|  |<------------------ A
/// -- 1..3 `/properties/A`               <--- 1..3 <-|  |<--------------- B
///      <type: object>                                  |            ]
///      <maxLength: 5>                                  |
/// -- 3..4 `/properties/B`               <--- 3..4 <----|
///      <type: string>
/// ]
/// ```
#[derive(Debug, Eq, PartialEq, Hash)]
pub(crate) struct RangedEdge {
    /// A label for this edge.
    pub(crate) label: EdgeLabel,
    /// A range of nodes referenced by this edge.
    pub(crate) nodes: Range<usize>,
}

impl RangedEdge {
    pub(crate) fn new(label: impl Into<EdgeLabel>, nodes: Range<usize>) -> RangedEdge {
        RangedEdge {
            label: label.into(),
            nodes,
        }
    }
}

/// A slot for a node in a tree.
pub(crate) struct NodeSlot {
    /// Unique node identifier.
    id: NodeId,
    /// Whether this slot was already used or not.
    state: SlotState,
}

#[derive(Debug, Eq, PartialEq)]
enum SlotState {
    /// Slot was not previously used.
    New,
    /// Slot is already used.
    Used,
}

impl NodeSlot {
    fn seen(id: NodeId) -> Self {
        Self {
            id,
            state: SlotState::Used,
        }
    }
    fn new(id: NodeId) -> Self {
        Self {
            id,
            state: SlotState::New,
        }
    }
    fn is_new(&self) -> bool {
        self.state == SlotState::New
    }
}

pub(crate) type VisitedMap = HashMap<*const Value, NodeId>;

/// Build a packed graph to represent JSON Schema.
pub(crate) fn build<'s>(
    schema: &'s Value,
    root: &'s Resolver,
    resolvers: &'s HashMap<&str, Resolver>,
) -> Result<CompressedRangeGraph> {
    // Convert `Value` to an adjacency list and add all remote nodes reachable from the root
    let adjacency_list = AdjacencyList::new(schema, root, resolvers)?;
    // Each JSON Schema is a set of keywords that may contain nested sub-schemas. As all of nodes
    // are ordered by the BFS traversal order, we can address each schema by a range of indexes:
    //   * Create nodes with the same structure as the adjacency list but put corresponding
    //     `Some(Keyword)` instances at places containing valid JSON Schema keywords and fill
    //     everything else with `None`.
    //   * Convert edges, so they point to ranges of nodes
    let range_graph = RangeGraph::try_from(&adjacency_list)?;
    // Remove empty nodes and adjust all indexes
    Ok(range_graph.compress())
}

#[derive(Debug)]
pub(crate) struct AdjacencyList<'s> {
    pub(crate) nodes: Vec<&'s Value>,
    pub(crate) edges: Vec<Vec<Edge>>,
    visited: VisitedMap,
}

impl<'s> AdjacencyList<'s> {
    fn new(
        schema: &'s Value,
        root: &'s Resolver,
        resolvers: &'s HashMap<&str, Resolver>,
    ) -> Result<Self> {
        let mut output = AdjacencyList::empty();
        // This is a Breadth-First-Search routine
        let mut queue = VecDeque::new();
        queue.push_back((Scope::new(root), NodeId(0), EdgeLabel::Index(0), schema));
        while let Some((mut scope, parent_id, label, node)) = queue.pop_front() {
            let slot = output.push(parent_id, label, node);
            if slot.is_new() {
                match node {
                    Value::Object(object) => {
                        scope.track_folder(object);
                        // FIXME: track schema / non schema properly. Maybe extend scope?
                        for (key, value) in object {
                            if key == "$ref" {
                                if let Value::String(reference) = value {
                                    match Reference::try_from(reference.as_str())? {
                                        Reference::Absolute(location) => {
                                            if let Some(resolver) = resolvers.get(location.as_str())
                                            {
                                                let (folders, resolved) =
                                                    resolver.resolve(reference)?;
                                                queue.push_back((
                                                    Scope::with_folders(resolver, folders),
                                                    slot.id,
                                                    key.into(),
                                                    resolved,
                                                ));
                                            } else {
                                                let (_, resolved) =
                                                    scope.resolver.resolve(reference)?;
                                                queue.push_back((
                                                    scope.clone(),
                                                    slot.id,
                                                    key.into(),
                                                    resolved,
                                                ));
                                            }
                                        }
                                        Reference::Relative(location) => {
                                            let mut resolver = scope.resolver;
                                            if !is_local(location) {
                                                let location =
                                                    scope.build_url(resolver.scope(), location)?;
                                                if !resolver.contains(location.as_str()) {
                                                    resolver = resolvers
                                                        .get(location.as_str())
                                                        .expect("Unknown reference");
                                                }
                                            };
                                            let (folders, resolved) = resolver.resolve(location)?;
                                            queue.push_back((
                                                Scope::with_folders(resolver, folders),
                                                slot.id,
                                                key.into(),
                                                resolved,
                                            ));
                                        }
                                    };
                                }
                            } else {
                                queue.push_back((scope.clone(), slot.id, key.into(), value));
                            }
                        }
                    }
                    Value::Array(items) => {
                        for (idx, item) in items.iter().enumerate() {
                            queue.push_back((scope.clone(), slot.id, idx.into(), item));
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(output)
    }

    /// Create an empty adjacency list.
    fn empty() -> Self {
        Self {
            // For simpler BFS implementation we put a dummy node in the beginning
            // This way we can assume there is always a parent node, even for the schema root
            nodes: vec![&Value::Null],
            edges: vec![vec![]],
            visited: VisitedMap::new(),
        }
    }

    /// Push a new node & an edge to it.
    fn push(&mut self, parent_id: NodeId, label: EdgeLabel, node: &'s Value) -> NodeSlot {
        let slot = match self.visited.entry(node) {
            Entry::Occupied(entry) => NodeSlot::seen(*entry.get()),
            Entry::Vacant(entry) => {
                // Insert a new node & empty edges for it
                let node_id = NodeId(self.nodes.len());
                self.nodes.push(node);
                self.edges.push(vec![]);
                entry.insert(node_id);
                NodeSlot::new(node_id)
            }
        };
        // Insert a new edge from `parent_id` to this node
        self.edges[parent_id.0].push(Edge::new(label, slot.id));
        slot
    }

    pub(crate) fn range_of(&self, target_id: usize) -> Range<usize> {
        let (start, end) = match self.edges[target_id].as_slice() {
            // Node has no edges
            [] => return 0..0,
            [edge] => (edge, edge),
            [start, .., end] => (start, end),
        };
        // We use non-inclusive ranges, but edges point to precise indexes, hence add 1
        start.target.value()..end.target.value() + 1
    }
}
// TODO: What about specialization? When should it happen? RangeGraph?

#[derive(Debug)]
pub(crate) struct RangeGraph {
    pub(crate) nodes: Vec<Option<Keyword>>,
    pub(crate) edges: Vec<Option<RangedEdge>>,
}

macro_rules! vec_of_nones {
    ($size:expr) => {
        (0..$size).map(|_| None).collect()
    };
}

impl TryFrom<&AdjacencyList<'_>> for RangeGraph {
    type Error = Error;

    fn try_from(input: &AdjacencyList<'_>) -> Result<Self> {
        let mut output = RangeGraph {
            nodes: vec_of_nones!(input.nodes.len()),
            edges: vec_of_nones!(input.edges.len()),
        };
        let mut visited = vec![false; input.nodes.len()];
        let mut queue = VecDeque::new();
        queue.push_back((NodeId(0), &input.edges[0]));
        while let Some((node_id, node_edges)) = queue.pop_front() {
            if visited[node_id.value()] {
                continue;
            }
            visited[node_id.value()] = true;
            // TODO: Properly track scope of schema/nonschema.
            //       Likely $ref should be schema -> schema, and others are schema -> non-schema
            // TODO: Maybe we can skip pushing edges from non-applicators? they will be no-op here,
            //       but could be skipped upfront
            for edge in node_edges {
                queue.push_back((edge.target, &input.edges[edge.target.value()]));
            }
            if !node_id.is_root() {
                for edge in node_edges {
                    let target_id = edge.target.value();
                    let value = input.nodes[target_id];
                    match edge.label.as_key() {
                        Some("maximum") => {
                            output.set_node(target_id, Maximum::build(value.as_u64().unwrap()));
                        }
                        Some("maxLength") => {
                            output.set_node(target_id, MaxLength::build(value.as_u64().unwrap()));
                        }
                        Some("minProperties") => {
                            output
                                .set_node(target_id, MinProperties::build(value.as_u64().unwrap()));
                        }
                        Some("type") => {
                            let type_value = match value.as_str().unwrap() {
                                "array" => ValueType::Array,
                                "boolean" => ValueType::Boolean,
                                "integer" => ValueType::Integer,
                                "null" => ValueType::Null,
                                "number" => ValueType::Number,
                                "object" => ValueType::Object,
                                "string" => ValueType::String,
                                _ => panic!("invalid type"),
                            };
                            output.set_node(target_id, Type::build(type_value));
                        }
                        Some("properties") => {
                            let edges = input.range_of(target_id);
                            output.set_node(target_id, Properties::build(edges));
                            output.set_many_edges(&input.edges[target_id], input);
                        }
                        Some("items") => {
                            // TODO: properly set edges & node
                            output.set_node(target_id, Items::build());
                        }
                        Some("allOf") => {
                            let edges = input.range_of(target_id);
                            output.set_node(target_id, AllOf::build(edges));
                            output.set_many_edges(&input.edges[target_id], input);
                        }
                        Some("$ref") => {
                            // TODO: Inline reference
                            let nodes = input.range_of(target_id);
                            output.set_node(target_id, Ref::build(nodes));
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(output)
    }
}

impl RangeGraph {
    fn set_node(&mut self, id: usize, keyword: Keyword) {
        self.nodes[id] = Some(keyword)
    }
    fn set_edge(&mut self, id: usize, label: EdgeLabel, nodes: Range<usize>) {
        self.edges[id] = Some(RangedEdge::new(label, nodes))
    }
    fn set_many_edges(&mut self, edges: &[Edge], input: &AdjacencyList) {
        for edge in edges {
            let id = edge.target.value();
            self.set_edge(id, edge.label.clone(), input.range_of(id));
        }
    }
    fn compress(self) -> CompressedRangeGraph {
        todo!()
    }
}

#[derive(Debug)]
pub(crate) struct CompressedRangeGraph {
    pub(crate) nodes: Vec<Keyword>,
    pub(crate) edges: Vec<RangedEdge>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum BuildScope {
    Schema,
    NonSchema,
}

#[derive(Clone)]
struct Scope<'s> {
    folders: Vec<&'s str>,
    resolver: &'s Resolver<'s>,
}

impl<'s> Scope<'s> {
    pub(crate) fn new(resolver: &'s Resolver) -> Self {
        Self::with_folders(resolver, vec![])
    }
    pub(crate) fn with_folders(resolver: &'s Resolver, folders: Vec<&'s str>) -> Self {
        Self { folders, resolver }
    }
    pub(crate) fn track_folder(&mut self, object: &'s Map<String, Value>) {
        // Some objects may change `$ref` behavior via the `$id` keyword
        if let Some(id) = id_of_object(object) {
            self.folders.push(id);
        }
    }

    pub(crate) fn build_url(&self, scope: &Url, reference: &str) -> Result<Url> {
        let folders = &self.folders;
        let mut location = scope.clone();
        if folders.len() > 1 {
            for folder in folders.iter().skip(1) {
                location = location.join(folder)?;
            }
        }
        Ok(location.join(reference)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        schema::resolving,
        testing::{assert_adjacency_list, assert_compressed_graph, assert_range_graph, load_case},
    };
    use test_case::test_case;

    #[test_case("boolean")]
    #[test_case("maximum")]
    #[test_case("properties")]
    #[test_case("properties-empty")]
    #[test_case("nested-properties")]
    #[test_case("multiple-nodes-each-layer")]
    #[test_case("not-a-keyword-validation")]
    #[test_case("not-a-keyword-ref")]
    #[test_case("ref-recursive-absolute")]
    #[test_case("ref-recursive-self")]
    #[test_case("ref-recursive-between-schemas")]
    #[test_case("ref-remote-pointer")]
    #[test_case("ref-remote-nested")]
    #[test_case("ref-remote-base-uri-change")]
    #[test_case("ref-remote-base-uri-change-folder")]
    #[test_case("ref-remote-base-uri-change-in-subschema")]
    #[test_case("ref-multiple-same-target")]
    fn internal_structure(name: &str) {
        let schema = &load_case(name)["schema"];
        let (root, external) = resolving::resolve(schema).unwrap();
        let resolvers = resolving::build_resolvers(&external);
        let adjacency_list = AdjacencyList::new(schema, &root, &resolvers).unwrap();
        assert_adjacency_list(&adjacency_list);
        let range_graph = RangeGraph::try_from(&adjacency_list).unwrap();
        assert_range_graph(&range_graph);
        let compressed = range_graph.compress();
        assert_compressed_graph(&compressed);
    }
}
