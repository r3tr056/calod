
# Calod: Durable Action Ledger for AI Agents (core: a Rust data store)

[![Project Status](https://img.shields.io/badge/status-beta-yellow)](https://github.com/r3tr056/calod) [![License](https://img.shields.io/badge/License-MIT-blue)](LICENSE) [![Benchmarks](https://img.shields.io/badge/Benchmarks-harness%20ready-orange)](calod/scripts/benchmark/README.md)

![Calod Logo Placeholder](https://github.com/r3tr056/calod/blob/dev/.github/images/logo.jpeg?raw=true) <!-- Replace with your actual logo image -->

**Calod is becoming the durable action ledger for AI agents: when an agent retries a tool call, Calod prevents the retry from repeating a real-world side effect, records a tamper-evident receipt, and makes uncertain outcomes recoverable. The storage core is a sharded Rust data store that speaks the Redis protocol (RESP).**

> Status: pre-release. The effect-ledger API is under active development (see `calod/docs/PRODUCT_STRATEGY_V2_REFINED.md`). The RESP data-store endpoint described below works today and is retained as a compatibility/inspection interface; it is not the primary product API.

## ✨ Introduction

Calod's core is a data store written in Rust, inspired by the Redis protocol and data model. On top of that core we are building the effect ledger: idempotent tool-execution coordination with durable receipts for AI agents that call payment, email, ticketing, CRM, and other mutation tools.

**What Calod is honest about today:**

*   **Performance:** engineered for low latency, but no reproducible benchmark numbers are published yet (see the Benchmarks section).
*   **Scalability:** sharding is in-process only. There is no clustering, replication, or failover — a single node is a single point of failure.
*   **Durability:** snapshot + checksummed, crash-safe WAL persistence with verified restore. Do not rely on it as your only copy of data yet.
*   **Redis compatibility:** Calod speaks RESP2/RESP3 and implements a large subset of Redis commands, but it is not a verified drop-in replacement. A compatibility matrix and the list of unsupported commands will be published with the first release.

## ⚡ Key Features

*   **Effect ledger (in development):** idempotency-keyed tool execution with leases, fencing tokens, and an explicit `outcome_unknown` state for crash recovery.
*   **Durable receipts:** append-only, hash-chained run timelines recording inputs, attempts, and outcomes of every protected action.
*   **Sharded Architecture:** in-process sharding for increased capacity and concurrency on a single node.
*   **RESP Protocol:** speaks the Redis protocol, so many existing Redis clients (Python, Go, Node.js, etc.) can connect for the compatibility endpoint.
*   **JSON Document Support:** Native support for storing and querying JSON documents with efficient path-based operations (JSON.SET, JSON.GET, JSON.DEL, etc.).
*   **String, Hash, List, Set Data Types:** Supports core Redis data types for versatile data storage.
*   **Pub/Sub:** Redis-style Publish/Subscribe messaging.
*   **Persistence Options:**  Snapshot and checksummed WAL persistence with verified restore.
*   **LRU Eviction Policy:** Efficient memory management with Least Recently Used eviction.
*   **Command Set:** Implements a large subset of Redis commands; a compatibility matrix will be published with the first release.
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

No benchmark numbers are published yet — the measurement harness is built in milestone M0 and results will be published once they exist. No fake numbers, ever.

- Benchmark methodology and runner (calod vs Redis 7.4 vs Valkey 8 at equal cores, memtier_benchmark): [`calod/scripts/benchmark/README.md`](calod/scripts/benchmark/README.md)
- Redis tcl-suite parity runner: [`calod/scripts/tcl-suite/README.md`](calod/scripts/tcl-suite/README.md)
- Python stress-workload driver (docker-based): [`calod/scripts/stress_runner.py`](calod/scripts/stress_runner.py), invoked via [`calod/scripts/run-stress.sh`](calod/scripts/run-stress.sh)

## 🗺️ Roadmap

*   **Effect ledger MVP:** idempotent tool execution, leases and fencing, `outcome_unknown` reconciliation, hash-chained run timelines.
*   **Python and TypeScript SDKs:** one decorator/middleware call to protect a tool function.
*   **Framework integrations:** LangGraph, CrewAI, OpenAI Agents SDK, generic MCP tool middleware.
*   **Evidence export:** retention policies plus auditor-friendly export of action receipts.
*   **Hardened durability:** binary-safe WAL, fail-closed crash recovery, automated backup-restore verification.
*   **Clustering and High Availability:** replication and failover, only after the single-node foundation is proven.

Not on the roadmap: vector search, semantic memory, a general agent framework, full workflow orchestration.

## 🤝 Contributing

We welcome contributions to Calod! Please see our [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines on how to contribute.

## 📜 License

Calod is licensed under the [MIT License](LICENSE).

## 💬 Community & Support

[Link to your community forum, chat, or contact information]

**Stay tuned for updates and join our community as we build the future of fast data storage!**
