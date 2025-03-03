use std::sync::Arc;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
	id: String,
	properties: DashMap<String, String>,
}

impl Node {
	pub fn new(id: String) -> Self {
		Node { id, properties: DashMap::new() }
	}

	pub fn get_id(&self) -> &str { &self.id }

	pub fn properties(&self) -> &DashMap<String, String> {
		&self.properties
	}

	pub fn set_property(&self, key: String, value: String) {
		self.properties.insert(key, value);
	}

	pub fn get_property(&self, key: &str) -> Option<String> {
		self.properties.get(key).map(|entry| entry.value().clone())
	}

	pub fn remove_property(&self, key: &str) {
		self.properties.remove(key);
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
	id: String,
	source_id: String,
	target_id: String,
	relation_type: String,
	properties: DashMap<String, String>,
}

impl Edge {
	pub fn new(id: String, source_id: String, target_id: String, relation_type: String) -> Self {
		Edge {
			id,
			source_id,
			target_id,
			relation_type,
			properties: DashMap::new(),
		}
	}

	pub fn get_id(&self) -> &str {
		&self.id
	}

	pub fn get_source_id(&self) -> &str {
		&self.source_id
	}

	pub fn get_target_id(&self) -> &str {
		&self.target_id
	}

	pub fn get_relation_type(&self) -> &str {
		&self.relation_type
	}

	pub fn properties(&self) -> &DashMap<String, String> {
		&self.properties
	}

	pub fn set_property(&self, key: String, value: String) {
		self.properties.insert(key, value);
	}

	pub fn get_property(&self, key: &str) -> Option<String> {
		self.properties.get(key).map(|entry| entry.value().clone())
	}

	pub fn remove_property(&self, key: &str) {
		self.properties.remove(key);
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GraphData {
    pub nodes: DashMap<String, Node>, // Key: Node ID, Value: Node
    pub edges: DashMap<String, Edge>, // Key: Edge ID, Value: Edge
}

impl GraphData {
    pub fn new() -> Self {
        GraphData::default() // Use Default implementation to initialize DashMaps
    }

    pub fn get_nodes(&self) -> &DashMap<String, Node> {
        &self.nodes
    }

    pub fn get_edges(&self) -> &DashMap<String, Edge> {
        &self.edges
    }

    pub fn add_node(&self, node: Node) {
        self.nodes.insert(node.get_id().to_string(), node);
    }

    pub fn get_node(&self, node_id: &str) -> Option<Arc<Node>> {
        self.nodes.get(node_id).map(|ref_val| Arc::new(ref_val.clone()))
    }

    pub fn remove_node(&self, node_id: &str) {
        self.nodes.remove(node_id);
    }

    pub fn add_edge(&self, edge: Edge) {
        self.edges.insert(edge.get_id().to_string(), edge);
    }

    pub fn get_edge(&self, edge_id: &str) -> Option<Arc<Edge>> {
        self.edges.get(edge_id).map(|ref_val| Arc::new(ref_val.clone()))
    }

    pub fn remove_edge(&self, edge_id: &str) {
        self.edges.remove(edge_id);
    }
}