
# Calod: Superfast, AI-Ready Data Store 🚀

[![Project Status](https://img.shields.io/badge/status-beta-yellow)](https://github.com/r3tr056/calod) [![License](https://img.shields.io/badge/License-MIT-blue)](LICENSE) [![Benchmarks](https://img.shields.io/badge/Benchmarks-Coming%20Soon-orange)](BENCHMARKS.md)

![Calod Logo Placeholder](https://github.com/r3tr056/calod/blob/dev/.github/images/logo.jpeg?raw=true) <!-- Replace with your actual logo image -->

**A blazing-fast, sharded, and extensible data store optimized for AI applications. Built for speed, reliability, and modern workloads.**

## ✨ Introduction

Calod is a next-generation data store engineered for extreme performance and the demands of modern AI and real-time applications.  Inspired by the speed and simplicity of Redis but built from the ground up in Rust, Calod delivers unparalleled speed, robust sharding for scalability, and advanced features tailored for AI workloads.

**Why Calod?**

*   **Unmatched Speed:**  Leveraging Rust's performance and low-level optimizations, Calod aims to be significantly faster than traditional key-value stores like Redis, especially in demanding scenarios.
*   **AI-First Design:**  Calod is not just fast; it's smart. We are actively building in features crucial for AI applications, including:
    *   **Document Data Support (JSON):** Store and query JSON documents natively with efficient path-based access.
    *   **Vector Search (Roadmap):**  Future support for high-dimensional vector embeddings and similarity search for AI similarity tasks.
    *   **Feature Store Capabilities:**  Designed for low-latency feature serving, essential for online machine learning inference.
*   **Scalable and Reliable:** Sharded architecture ensures horizontal scalability to handle massive datasets and high throughput. Persistence options and high-availability features are being developed for production stability.
*   **Extensible and Modern:** Built in Rust, Calod is designed for extensibility and integrates seamlessly with modern cloud-native environments.

## ⚡ Key Features

*   **Blazing Fast Performance:** Optimized for low latency and high throughput using Rust and advanced data structures.
*   **Sharded Architecture:** Horizontally scalable across multiple shards for increased capacity and concurrency.
*   **Redis-Compatible Protocol:** Speaks the RESP protocol, allowing seamless integration with existing Redis clients (Python, Go, Node.js, etc.).
*   **JSON Document Support:** Native support for storing and querying JSON documents with efficient path-based operations (JSON.SET, JSON.GET, JSON.DEL, etc.).
*   **String, Hash, List, Set Data Types:** Supports core Redis data types for versatile data storage.
*   **Pub/Sub (Roadmap):**  Future support for Redis-style Publish/Subscribe messaging patterns.
*   **Persistence Options:**  RDB and AOF persistence for data durability.
*   **LRU Eviction Policy:** Efficient memory management with Least Recently Used eviction.
*   **Comprehensive Command Set:** Implements a growing subset of Redis commands, with a focus on essential and AI-relevant operations.
*   **Metrics and Monitoring:**  Exposes detailed metrics for performance monitoring and observability.
*   **Client Connection Management:** Robust handling of client connections with features like `CLIENT LIST`, `CLIENT KILL`, `CLIENT GETNAME`, `CLIENT SETNAME`.

## 🚀 Getting Started

### Prerequisites

*   **Rust Toolchain:**  Install Rust from [rustup.rs](https://rustup.rs/).
*   **Cargo:** Rust's build system and package manager (included with Rust).

### Build from Source

```bash
git clone https://github.com/r3tr056/calod.git
cd calod
cargo build --release
```

The compiled binary will be located at `target/release/calod`.

### Run Calod Server

```bash
target/release/calod
```

Calod server will start listening on `127.0.0.1:6379` by default.

### Connect with a Redis Client (Python Example)

```python
import redis

try:
    r = redis.Redis(host='localhost', port=6379, db=0)
    r.ping()
    print("Successfully connected to Calod server!")

    # Example: Setting and Getting a String
    r.set('mykey', 'Hello Calod!')
    value = r.get('mykey')
    print(f"Value for 'mykey': {value.decode('utf-8')}")

    # Example: JSON Operations
    r.execute_command('JSON.SET', 'mydoc', '.', '{"name": "Calod", "type": "Data Store"}')
    doc = r.execute_command('JSON.GET', 'mydoc', '.')
    print(f"JSON Document: {doc.decode('utf-8')}")
    doc_name = r.execute_command('JSON.GET', 'mydoc', '.name')
    print(f"JSON Document Name: {doc_name.decode('utf-8')}")


except redis.exceptions.ConnectionError as e:
    print(f"Connection Error: {e}")
    print("Make sure Calod server is running on localhost:6379")
```

## 💡 Usage Examples

### Basic Key-Value Operations (Redis-style)

```python
import redis
r = redis.Redis(host='localhost', port=6379, db=0)

r.set('user:123', 'John Doe')
username = r.get('user:123').decode('utf-8')
print(f"Username: {username}")

r.hset('user:123:profile', mapping={'email': 'john.doe@example.com', 'city': 'New York'})
email = r.hget('user:123:profile', 'email').decode('utf-8')
print(f"Email: {email}")

r.lpush('tasks', ['Task A', 'Task B', 'Task C'])
task = r.lpop('tasks').decode('utf-8')
print(f"Task popped: {task}")
```

### JSON Document Operations

```python
import redis
r = redis.Redis(host='localhost', port=6379, db=0)

r.execute_command('JSON.SET', 'product:456', '.', '''
{
  "name": "AI-Powered Smart Widget",
  "description": "A revolutionary widget enhanced with AI.",
  "price": 99.99,
  "features": ["Smart Recommendations", "Personalized Experience", "Predictive Analytics"]
}
''')

product_name = r.execute_command('JSON.GET', 'product:456', '.name').decode('utf-8')
print(f"Product Name: {product_name}")

price = r.execute_command('JSON.GET', 'product:456', '.price').decode('utf-8')
print(f"Price: {price}")

r.execute_command('JSON.NUMINCRBY', 'product:456', '.price', 10.0)
updated_price = r.execute_command('JSON.GET', 'product:456', '.price').decode('utf-8')
print(f"Updated Price: {updated_price}")

r.execute_command('JSON.ARRAPPEND', 'product:456', '.features', '["Cloud Integration", "Voice Control"]')
updated_features = r.execute_command('JSON.GET', 'product:456', '.features')
print(f"Updated Features: {updated_features.decode('utf-8')}")
```

## 📊 Performance Benchmarks

[BENCHMARKS.md](BENCHMARKS.md)

**Performance benchmarks are currently being finalized and will be available soon.** We are targeting significant performance improvements over existing solutions.  We will showcase benchmarks comparing Calod against Redis and other data stores under various workloads.

**[Coming Soon: Link to detailed benchmark results and methodology]**

## 🗺️ Roadmap

*   **Vector Search and Embeddings:** Implement native vector data type and ANN indexing for similarity search.
*   **Pub/Sub Functionality:**  Add Redis-compatible Publish/Subscribe features.
*   **Clustering and High Availability:** Implement clustering and replication for production deployments.
*   **Enhanced Persistence Options:** Explore more advanced persistence and backup/restore mechanisms.
*   **More Redis Command Coverage:**  Continue to expand the set of Redis-compatible commands.
*   **Client Libraries:** Develop and enhance client libraries for popular languages.

## 🤝 Contributing

We welcome contributions to Calod! Please see our [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines on how to contribute.

## 📜 License

Calod is licensed under the [MIT License](LICENSE).

## 💬 Community & Support

[Link to your community forum, chat, or contact information]

**Stay tuned for updates and join our community as we build the future of fast data storage!**
