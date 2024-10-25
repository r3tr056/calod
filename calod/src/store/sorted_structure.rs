use std::sync::{Arc, RwLock};
use std::cmp::Ordering;

/*
Requirements:
1. Concurrency Model: Since a Red-Black tree involves frequent rebalancing, a naive Mutex based apporach is inefficient for highly concurrent applications. We can use a Fine-grained Locking strategy

2. Rust Features: Rust's ownership and borrowing model with strict lifetimes checks will require a careful choice of sync primitives. Structures like `Arc` (Atomic Reference Counter) for shared ownership and `RWLock` for read-write access control will be essential

3. Concurrent access: Mutliple users must be able to access and store data concurrently, the data struct should support concurrent reads and writes without severe contention issues.

4. Logarthmic Complexity for Insertion, Deletion and Lookup : A balanced tree structure like the RB Tree offers O(log N) time complexity for insertion, deletions and lookups, similar to Redi's Skip List, enabling efficient management event under large datasets

5. Rank-based Queries and Ranged Operations: Redis sorted sets provide commands like ZRANK, ZRANGE, ZREVRANK, ZREBRANGE, which rely on the ablity to determine the rank of an item or retreive a range of items by rank. A RB tree maintains order internally, allowing range queries and rank-based retreivals to be performed efficiently

6. Expiration Management (TTL): The sorted data structure should also support TTL (time-to-live) by maintaining order by expiration time or periodically check expirations and evicting expired items.


*/

#[derive(PartialEq, Eq, Debug)]
enum Color {
	Red,
	Black,
}

struct Node<T> {
	value: T,
	color: Color,
	left: Option<Arc<RwLock<Node<T>>>>,
	right: Option<Arc<RwLock<Node<T>>>>,
	parent: Option<Arc<RwLock<Node<T>>>>,
}

impl<T> Node<T> {
	fn new(value: T, color: Color) -> Self {
		Node {
			value,
			color,
			left: None,
			right: None,
			parent: None,
		}
	}
}

struct ConcurrentRBTree<T> {
	root: Option<Arc<RwLock<Node<T>>>>,
}

impl<T: Ord + Clone> ConcurrentRBTree<T> {
	fn new() -> Self {
		ConcurrentRBTree { root: None }
	}

	fn insert(&mut self, value: T) {
		match self.root {
			Some(ref root) => self.insert_rec(root.clone(), value),
			None => {
				let new_node = Arc::new(RwLock::new(Node::new(value, Color::Black)));
				self.root = Some(new_node);
			}
		}
	}

	fn insert_rec(&self, current: Arc<RwLock<Node<T>>>, value: T) {
		let mut current_guard = current.write().unwrap();

		match value.cmp(&current_guard.value) {
			Ordering::Less => {
				if let Some(ref left) = current_guard.left {
					drop(current_guard);
					self.insert_rec(left.clone(), value);
				} else {
					let new_node = Arc::new()
				}
			}
		}
	}
}