import json
import os
import random
import string
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

import redis


def env_int(name: str, default: int) -> int:
    raw = os.getenv(name)
    if raw is None or raw == "":
        return default
    return int(raw)


def percentile(values, pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, int(round((pct / 100.0) * (len(ordered) - 1)))))
    return ordered[index]


def median(values: list[float]) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2 == 1:
        return ordered[middle]
    return (ordered[middle - 1] + ordered[middle]) / 2.0


def now_utc() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def as_text(value) -> str:
    if isinstance(value, bytes):
        return value.decode("utf-8")
    return str(value)


def summarize_repeated_results(results: list[dict]) -> dict:
    timed = [item for item in results if item.get("mode") == "timed"]
    if not timed:
        return {"runs": len(results)}

    return {
        "runs": len(timed),
        "operations_total": sum(item.get("operations_total", 0) for item in timed),
        "errors_total": sum(item.get("errors_total", 0) for item in timed),
        "ops_per_sec_median": round(median([item.get("ops_per_sec", 0.0) for item in timed]), 2),
        "latency_ms_median": {
            "p50": round(median([item.get("latency_ms", {}).get("p50", 0.0) for item in timed]), 3),
            "p95": round(median([item.get("latency_ms", {}).get("p95", 0.0) for item in timed]), 3),
            "p99": round(median([item.get("latency_ms", {}).get("p99", 0.0) for item in timed]), 3),
        },
    }


class StressHarness:
    def __init__(self) -> None:
        self.profile = os.getenv("STRESS_PROFILE", "A").strip().upper()
        self.target_name = os.getenv("STRESS_TARGET_NAME", "calod").strip().lower()
        self.host = os.getenv("REDIS_HOST", "127.0.0.1")
        self.port = env_int("REDIS_PORT", 6379)
        self.artifact_path = Path(os.getenv("STRESS_ARTIFACT_PATH", f"/artifacts/{self.profile}-results.json"))
        self.seed = env_int("STRESS_RANDOM_SEED", 42)
        self.repeat_runs = env_int("STRESS_REPEAT_RUNS", 1)
        self.client = redis.Redis(host=self.host, port=self.port, socket_timeout=5, decode_responses=False)
        self.client.set_response_callback("COMMAND", lambda response, **_kwargs: response)
        self.client.set_response_callback("HINCRBYFLOAT", lambda response, **_kwargs: response)
        self._local = threading.local()

    def connect(self) -> None:
        deadline = time.time() + 60
        last_error = None
        while time.time() < deadline:
            try:
                self.client.ping()
                return
            except Exception as exc:  # noqa: BLE001
                last_error = str(exc)
                time.sleep(1)
        raise RuntimeError(f"calod did not become ready: {last_error}")

    def write_result(self, payload: dict) -> None:
        self.artifact_path.parent.mkdir(parents=True, exist_ok=True)
        self.artifact_path.write_text(json.dumps(payload, indent=2), encoding="utf-8")

    def info_snapshot(self) -> dict:
        try:
            return self.client.info()
        except Exception as exc:  # noqa: BLE001
            return {"error": str(exc)}

    def get_client(self) -> redis.Redis:
        client = getattr(self._local, "client", None)
        if client is None:
            client = redis.Redis(host=self.host, port=self.port, socket_timeout=5, decode_responses=False)
            client.set_response_callback("COMMAND", lambda response, **_kwargs: response)
            client.set_response_callback("HINCRBYFLOAT", lambda response, **_kwargs: response)
            self._local.client = client
        return client

    def run(self) -> int:
        self.connect()
        if self.repeat_runs > 1 and self.profile in {"A", "B", "C"}:
            return self.run_repeated()

        result = {
            "profile": self.profile,
            "target_name": self.target_name,
            "started_at": now_utc(),
            "target": f"{self.host}:{self.port}",
        }
        if self.profile == "SMOKE":
            result.update(self.run_profile_smoke())
        elif self.profile == "A":
            result.update(self.run_profile_a())
        elif self.profile == "B":
            result.update(self.run_profile_b())
        elif self.profile == "C":
            result.update(self.run_profile_c())
        elif self.profile == "RELEASE":
            result.update(self.run_profile_release())
        else:
            raise ValueError(f"unsupported STRESS_PROFILE={self.profile}")

        result["finished_at"] = now_utc()
        result["info"] = self.info_snapshot()
        self.write_result(result)
        print(json.dumps(result, indent=2))
        return 0

    def run_repeated(self) -> int:
        started_at = now_utc()
        runs = []
        base_seed = self.seed
        for run_index in range(1, self.repeat_runs + 1):
            self.seed = base_seed + run_index - 1
            result = {
                "profile": self.profile,
                "target_name": self.target_name,
                "started_at": now_utc(),
                "target": f"{self.host}:{self.port}",
                "run_index": run_index,
                "runs_requested": self.repeat_runs,
            }
            if self.profile == "A":
                result.update(self.run_profile_a())
            elif self.profile == "B":
                result.update(self.run_profile_b())
            elif self.profile == "C":
                result.update(self.run_profile_c())
            else:
                raise ValueError(f"unsupported STRESS_PROFILE={self.profile}")

            result["finished_at"] = now_utc()
            result["info"] = self.info_snapshot()
            runs.append(result)

        payload = {
            "profile": self.profile,
            "target_name": self.target_name,
            "started_at": started_at,
            "finished_at": now_utc(),
            "target": f"{self.host}:{self.port}",
            "mode": "repeated",
            "runs_requested": self.repeat_runs,
            "summary": summarize_repeated_results(runs),
            "runs": runs,
        }
        self.write_result(payload)
        print(json.dumps(payload, indent=2))
        return 0

    def run_profile_smoke(self) -> dict:
        client = self.client
        client.flushall()
        checks = []

        client.ping()
        checks.append("PING")

        assert client.set("smoke:string", b"value") is True
        assert client.get("smoke:string") == b"value"
        checks.append("SET/GET")

        assert client.exists("smoke:string") == 1
        assert client.expire("smoke:string", 60) is True
        assert client.ttl("smoke:string") >= 0
        checks.append("EXISTS/EXPIRE/TTL")

        assert client.hset("smoke:hash", mapping={"field": "value"}) >= 1
        assert client.hget("smoke:hash", "field") == b"value"
        checks.append("HSET/HGET")

        assert client.lpush("smoke:list", b"a", b"b") == 2
        assert client.rpop("smoke:list") == b"a"
        checks.append("LPUSH/RPOP")

        assert client.sadd("smoke:set", b"a", b"b") == 2
        members = client.smembers("smoke:set")
        assert b"a" in members and b"b" in members
        checks.append("SADD/SMEMBERS")

        assert client.execute_command("TOUCH", "smoke:set") == 1
        assert client.execute_command("DBSIZE") >= 1
        checks.append("TOUCH/DBSIZE")

        return {
            "mode": "smoke",
            "checks": checks,
            "checks_total": len(checks),
            "dbsize_after": client.dbsize(),
        }

    def run_profile_release(self) -> dict:
        client = self.client
        client.flushall()
        checks = []

        self.record_check(checks, "server/introspection", self.validate_server_commands)
        self.record_check(checks, "strings/numbers", self.validate_string_commands)
        self.record_check(checks, "generic-keyspace", self.validate_keyspace_commands)
        self.record_check(checks, "hashes", self.validate_hash_commands)
        self.record_check(checks, "lists", self.validate_list_commands)
        self.record_check(checks, "sets", self.validate_set_commands)
        self.record_check(checks, "json", self.validate_json_commands)
        self.record_check(checks, "graph", self.validate_graph_commands)
        self.record_check(checks, "error-paths", self.validate_error_paths)

        return {
            "mode": "release-validation",
            "checks": checks,
            "checks_total": len(checks),
            "dbsize_after": client.dbsize(),
        }

    def record_check(self, checks: list[str], name: str, validator) -> None:
        validator(self.client)
        checks.append(name)

    def assert_ok(self, value, message: str = "expected OK") -> None:
        assert value is True or value == b"OK" or value == "OK", f"{message}, got {value!r}"

    def assert_response_error(self, command, expected_substring: str) -> None:
        try:
            command()
        except redis.ResponseError as exc:
            assert expected_substring in str(exc), f"expected {expected_substring!r} in {exc!r}"
            return
        raise AssertionError(f"expected ResponseError containing {expected_substring!r}")

    def validate_server_commands(self, client: redis.Redis) -> None:
        assert client.ping() is True
        assert client.execute_command("ECHO", "release") == b"release"
        assert client.execute_command("HELLO", 2)
        self.assert_ok(client.execute_command("CLIENT", "SETNAME", "release-validator"))
        assert client.execute_command("CLIENT", "GETNAME") == b"release-validator"
        assert b"release-validator" in client.execute_command("CLIENT", "INFO")
        assert b"addr=" in client.execute_command("CLIENT", "LIST")
        assert client.execute_command("COMMAND", "LIST")
        assert client.execute_command("COMMAND", "INFO", "SET")
        assert client.execute_command("COMMAND", "HELP", "SET")
        info = client.info()
        assert "calod_version" in info or "redis_version" in info
        assert "used_memory" in client.info("memory")
        self.assert_ok(client.execute_command("SELECT", 0))
        self.assert_response_error(
            lambda: client.execute_command("SELECT", 1),
            "DB index is out of range",
        )
        assert client.execute_command("WAIT", 1, 1) == 0

    def validate_string_commands(self, client: redis.Redis) -> None:
        assert client.set("release:string", b"value") is True
        assert client.get("release:string") == b"value"
        assert client.execute_command("SET", "release:setnx", "first", "NX") is True
        assert client.execute_command("SET", "release:setnx", "second", "NX") is None
        assert client.execute_command("SET", "release:setnx", "second", "XX") is True
        assert client.get("release:setnx") == b"second"
        assert client.execute_command("SETNX", "release:setnx2", "v") in (1, True)
        assert client.execute_command("SETEX", "release:setex", 60, "v") is True
        assert client.execute_command("PSETEX", "release:psetex", 60000, "v") is True
        assert client.execute_command("GETEX", "release:setex", "PERSIST") == b"v"
        assert client.ttl("release:setex") == -1
        assert client.execute_command("GETSET", "release:string", "new") == b"value"
        assert client.execute_command("GETDEL", "release:string") == b"new"
        assert client.get("release:string") is None
        assert client.execute_command("MSET", "release:m1", "a", "release:m2", "b") is True
        assert client.mget("release:m1", "release:m2", "release:missing") == [b"a", b"b", None]
        assert client.execute_command("MSETNX", "release:m3", "c", "release:m4", "d") in (1, True)
        assert client.execute_command("MSETNX", "release:m3", "x", "release:m5", "e") in (0, False)
        assert client.execute_command("APPEND", "release:append", "abc") == 3
        assert client.execute_command("APPEND", "release:append", "def") == 6
        assert client.execute_command("STRLEN", "release:append") == 6
        assert client.execute_command("GETRANGE", "release:append", 1, 3) == b"bcd"
        assert client.execute_command("SETRANGE", "release:append", 3, "XYZ") == 6
        assert client.get("release:append") == b"abcXYZ"
        assert client.execute_command("INCR", "release:num") == 1
        assert client.execute_command("INCRBY", "release:num", 4) == 5
        assert client.execute_command("DECR", "release:num") == 4
        assert client.execute_command("DECRBY", "release:num", 2) == 2
        assert float(client.execute_command("INCRBYFLOAT", "release:float", "1.5")) == 1.5

    def validate_keyspace_commands(self, client: redis.Redis) -> None:
        assert client.set("release:key", b"value") is True
        assert client.execute_command("TYPE", "release:key") == b"string"
        assert b"release:key" in client.execute_command("KEYS", "release:*")
        assert client.exists("release:key", "release:missing") == 1
        assert client.execute_command("TOUCH", "release:key", "release:missing") == 1
        assert client.execute_command("COPY", "release:key", "release:key-copy") == 1
        assert client.get("release:key-copy") == b"value"
        assert client.execute_command("RENAMENX", "release:key-copy", "release:key-copy2") == 1
        assert client.execute_command("RENAME", "release:key-copy2", "release:key-renamed") is True
        assert client.get("release:key-renamed") == b"value"
        assert client.execute_command("EXPIRE", "release:key", 60) == 1
        assert client.ttl("release:key") >= 0
        assert client.execute_command("PEXPIRE", "release:key", 60000) == 1
        assert client.pttl("release:key") >= 0
        assert client.execute_command("EXPIREAT", "release:key", int(time.time()) + 60) == 1
        assert client.execute_command("PEXPIREAT", "release:key", int(time.time() * 1000) + 60000) == 1
        assert client.execute_command("EXPIRETIME", "release:key") > 0
        assert client.execute_command("PEXPIRETIME", "release:key") > 0
        assert client.execute_command("PERSIST", "release:key") == 1
        assert client.ttl("release:key") == -1
        assert client.execute_command("OBJECT", "ENCODING", "release:key")
        assert client.execute_command("OBJECT", "REFCOUNT", "release:key") == 1
        assert client.execute_command("OBJECT", "IDLETIME", "release:key") >= 0
        cursor, keys = client.scan(0, match="release:*", count=100)
        assert isinstance(cursor, int)
        assert b"release:key" in keys
        assert client.randomkey() is not None
        assert client.dbsize() > 0
        assert client.execute_command("UNLINK", "release:key-renamed") == 1
        assert client.delete("release:key") == 1

    def validate_hash_commands(self, client: redis.Redis) -> None:
        assert client.hset("release:hash", mapping={"a": "1", "b": "2"}) == 2
        assert client.hget("release:hash", "a") == b"1"
        assert client.execute_command("HEXISTS", "release:hash", "a") == 1
        assert client.execute_command("HLEN", "release:hash") == 2
        assert client.execute_command("HMGET", "release:hash", "a", "missing") == [b"1", None]
        hkeys = client.execute_command("HKEYS", "release:hash")
        assert b"a" in hkeys and b"b" in hkeys
        hvals = client.execute_command("HVALS", "release:hash")
        assert b"1" in hvals and b"2" in hvals
        hgetall = client.hgetall("release:hash")
        assert hgetall[b"a"] == b"1"
        assert client.execute_command("HINCRBY", "release:hash", "n", 2) == 2
        hincr_float = client.execute_command("HINCRBYFLOAT", "release:hash", "f", "1.25")
        assert b"f" in hincr_float and b"1.25" in hincr_float
        assert client.execute_command("HSETNX", "release:hash", "a", "changed") == 0
        assert client.execute_command("HSETNX", "release:hash", "c", "3") == 1
        self.assert_ok(client.execute_command("HMSET", "release:hash", "d", "4", "e", "5"))
        assert client.hget("release:hash", "d") == b"4"
        assert client.execute_command("HDEL", "release:hash", "a", "missing") == 1

    def validate_list_commands(self, client: redis.Redis) -> None:
        assert client.execute_command("RPUSH", "release:list", "a", "b", "c") == 3
        assert client.execute_command("LPUSH", "release:list", "z") == 4
        assert client.execute_command("LLEN", "release:list") == 4
        assert client.execute_command("LRANGE", "release:list", 0, -1) == [b"z", b"a", b"b", b"c"]
        assert client.execute_command("LINDEX", "release:list", 1) == b"a"
        assert client.execute_command("LINSERT", "release:list", "AFTER", "a", "x") == 5
        assert client.execute_command("LSET", "release:list", 0, "head") is True
        assert client.execute_command("LTRIM", "release:list", 0, 3) is True
        assert client.execute_command("LREM", "release:list", 0, "x") == 1
        assert client.execute_command("RPOPLPUSH", "release:list", "release:list2") == b"b"
        assert client.execute_command("BLPOP", "release:list", 1)[0] == b"release:list"
        assert client.execute_command("BRPOP", "release:list2", 1)[0] == b"release:list2"
        assert client.execute_command("RPOP", "release:list") == b"a"
        assert client.execute_command("LPOP", "release:list") is None

    def validate_set_commands(self, client: redis.Redis) -> None:
        assert client.sadd("release:set", "a", "b", "a") == 2
        assert client.execute_command("SCARD", "release:set") == 2
        assert client.execute_command("SISMEMBER", "release:set", "a") == 1
        members = client.smembers("release:set")
        assert b"a" in members and b"b" in members
        assert client.execute_command("SRANDMEMBER", "release:set") in members
        assert len(client.execute_command("SRANDMEMBER", "release:set", 2)) <= 2
        assert len(client.execute_command("SPOP", "release:set", 1)) == 1
        assert client.execute_command("SREM", "release:set", "a", "missing") in (0, 1)

    def validate_json_commands(self, client: redis.Redis) -> None:
        self.assert_ok(
            client.execute_command(
                "JSON.SET",
                "release:json",
                "$",
                '{"num":1,"text":"hi","arr":[1,2],"obj":{"a":1}}',
            )
        )
        assert json.loads(client.execute_command("JSON.GET", "release:json", "$.num")) == 1
        assert client.execute_command("JSON.TYPE", "release:json", "$.obj") == b"object"
        assert json.loads(client.execute_command("JSON.NUMINCRBY", "release:json", "$.num", "2")) == 3
        assert json.loads(client.execute_command("JSON.STRAPPEND", "release:json", "$.text", " there")) == "hi there"
        assert json.loads(client.execute_command("JSON.ARRAPPEND", "release:json", "$.arr", "3", "4")) == [1, 2, 3, 4]
        assert client.execute_command("JSON.ARRLEN", "release:json", "$.arr") == 4
        assert client.execute_command("JSON.ARRINDEX", "release:json", "$.arr", "3") == 2
        assert json.loads(client.execute_command("JSON.ARRINSERT", "release:json", "$.arr", 1, "9")) == [1, 9, 2, 3, 4]
        assert json.loads(client.execute_command("JSON.ARRPOP", "release:json", "$.arr", 1)) == 9
        assert client.execute_command("JSON.ARRTRIM", "release:json", "$.arr", 0, 1) == 2
        assert json.loads(client.execute_command("JSON.OBJSET", "release:json", "$.obj", "b", "2"))["b"] == 2
        obj_keys = client.execute_command("JSON.OBJKEYS", "release:json", "$.obj")
        assert b"a" in obj_keys and b"b" in obj_keys
        assert client.execute_command("JSON.OBJLEN", "release:json", "$.obj") == 2
        assert client.execute_command("JSON.DEL", "release:json", "$.obj.b") == 1

    def validate_graph_commands(self, client: redis.Redis) -> None:
        self.assert_ok(client.execute_command("GRAPH.CREATE_NODE", "release:graph", "n1", "name", "one"))
        self.assert_ok(client.execute_command("GRAPH.CREATE_NODE", "release:graph", "n2", "name", "two"))
        node = json.loads(client.execute_command("GRAPH.GET_NODE", "release:graph", "n1"))
        assert node["id"] == "n1"
        assert node["properties"]["name"] == "one"
        self.assert_ok(client.execute_command("GRAPH.SET_NODE_PROPERTY", "release:graph", "n1", "color", "blue"))
        node_props = json.loads(client.execute_command("GRAPH.GET_NODE_PROPERTIES", "release:graph", "n1"))
        assert node_props["color"] == "blue"
        assert client.execute_command("GRAPH.DELETE_NODE_PROPERTY", "release:graph", "n1", "color") == 1
        self.assert_ok(
            client.execute_command(
                "GRAPH.CREATE_EDGE",
                "release:graph",
                "e1",
                "n1",
                "n2",
                "knows",
                "since",
                "2026",
            )
        )
        edge = json.loads(client.execute_command("GRAPH.GET_EDGE", "release:graph", "e1"))
        assert edge["id"] == "e1"
        assert edge["source_id"] == "n1"
        assert edge["target_id"] == "n2"
        self.assert_ok(client.execute_command("GRAPH.SET_EDGE_PROPERTY", "release:graph", "e1", "weight", "1"))
        edge_props = json.loads(client.execute_command("GRAPH.GET_EDGE_PROPERTIES", "release:graph", "e1"))
        assert edge_props["weight"] == "1"
        assert client.execute_command("GRAPH.DELETE_EDGE_PROPERTY", "release:graph", "e1", "weight") == 1
        assert client.execute_command("GRAPH.DELETE_EDGE", "release:graph", "e1") == 1
        assert client.execute_command("GRAPH.DELETE_NODE", "release:graph", "n2") == 1

    def validate_error_paths(self, client: redis.Redis) -> None:
        self.assert_response_error(lambda: client.execute_command("NO_SUCH_COMMAND"), "unknown command")
        self.assert_response_error(lambda: client.execute_command("MSETNX"), "key value pairs")
        self.assert_response_error(
            lambda: client.execute_command("RENAME", "release:missing", "release:new"),
            "not found",
        )

    def run_profile_a(self) -> dict:
        defaults = {
            "duration_secs": 60,
            "concurrency": 32,
            "key_size_min": 16,
            "key_size_max": 32,
            "value_size_min": 32,
            "value_size_max": 128,
        }
        config = {
            **defaults,
            "duration_secs": env_int("STRESS_DURATION_SECS", defaults["duration_secs"]),
            "concurrency": env_int("STRESS_CONCURRENCY", defaults["concurrency"]),
        }
        return self.run_timed_profile("A", config, [("GET", 70), ("SET", 20), ("EXISTS", 10)])

    def run_profile_b(self) -> dict:
        defaults = {
            "duration_secs": 90,
            "concurrency": 64,
            "key_size_min": 16,
            "key_size_max": 32,
            "value_size_min": 32,
            "value_size_max": 256,
        }
        config = {
            **defaults,
            "duration_secs": env_int("STRESS_DURATION_SECS", defaults["duration_secs"]),
            "concurrency": env_int("STRESS_CONCURRENCY", defaults["concurrency"]),
        }
        return self.run_timed_profile(
            "B",
            config,
            [
                ("SET", 30),
                ("GET", 30),
                ("DEL", 5),
                ("UNLINK", 5),
                ("EXPIRE", 5),
                ("PEXPIRE", 5),
                ("RENAME", 5),
                ("RENAMENX", 5),
                ("TOUCH", 5),
                ("RANDOMKEY", 5),
            ],
        )

    def run_timed_profile(self, name: str, config: dict, weights: list[tuple[str, int]]) -> dict:
        self.client.flushall()
        seed_keys = env_int("STRESS_SEED_KEYS", max(config["concurrency"] * 64, 2048))
        rng = random.Random(self.seed)
        alphabet = string.ascii_lowercase + string.digits

        key_pool = [
            f"stress:{name}:{i}:{''.join(rng.choices(alphabet, k=rng.randint(config['key_size_min'], config['key_size_max'])))}"
            for i in range(seed_keys)
        ]
        for index, key in enumerate(key_pool):
            value = self.random_value(rng, config["value_size_min"], config["value_size_max"])
            self.client.set(key, value)
            if index % 512 == 0:
                self.client.ping()

        deadline = time.time() + config["duration_secs"]
        operations = [name for name, weight in weights for _ in range(weight)]
        started = time.perf_counter()

        with ThreadPoolExecutor(max_workers=config["concurrency"]) as pool:
            futures = [
                pool.submit(self.worker_loop, worker_id, deadline, key_pool, operations, config)
                for worker_id in range(config["concurrency"])
            ]

        latencies = []
        total_ops = 0
        total_errors = 0
        op_counts = {}
        error_counts = {}
        for future in as_completed(futures):
            worker = future.result()
            latencies.extend(worker["latencies_ms"])
            total_ops += worker["ops"]
            total_errors += worker["errors"]
            for op_name, count in worker["op_counts"].items():
                op_counts[op_name] = op_counts.get(op_name, 0) + count
            for error_name, count in worker["error_counts"].items():
                error_counts[error_name] = error_counts.get(error_name, 0) + count

        elapsed = max(0.001, time.perf_counter() - started)
        return {
            "mode": "timed",
            "config": config,
            "seed_keys": seed_keys,
            "op_mix": {name: weight for name, weight in weights},
            "operations_total": total_ops,
            "errors_total": total_errors,
            "error_rate": round(total_errors / total_ops, 6) if total_ops else 0.0,
            "ops_per_sec": round(total_ops / elapsed, 2),
            "latency_ms": {
                "p50": round(percentile(latencies, 50), 3),
                "p95": round(percentile(latencies, 95), 3),
                "p99": round(percentile(latencies, 99), 3),
            },
            "per_operation": op_counts,
            "errors_by_type": error_counts,
            "dbsize_after": self.client.dbsize(),
        }

    def worker_loop(
        self,
        worker_id: int,
        deadline: float,
        key_pool: list[str],
        operations: list[str],
        config: dict,
    ) -> dict:
        rng = random.Random(self.seed + worker_id)
        client = self.get_client()
        latencies_ms = []
        ops = 0
        errors = 0
        op_counts = {}
        error_counts = {}

        while time.time() < deadline:
            op_name = rng.choice(operations)
            key = rng.choice(key_pool)
            started = time.perf_counter()
            try:
                self.execute_operation(client, rng, op_name, key, key_pool, config)
            except Exception as exc:  # noqa: BLE001
                errors += 1
                error_name = f"{op_name}:{type(exc).__name__}:{str(exc)[:160]}"
                error_counts[error_name] = error_counts.get(error_name, 0) + 1
            finally:
                elapsed_ms = (time.perf_counter() - started) * 1000.0
                latencies_ms.append(elapsed_ms)
                ops += 1
                op_counts[op_name] = op_counts.get(op_name, 0) + 1

        return {
            "ops": ops,
            "errors": errors,
            "latencies_ms": latencies_ms,
            "op_counts": op_counts,
            "error_counts": error_counts,
        }

    def execute_operation(
        self,
        client: redis.Redis,
        rng: random.Random,
        op_name: str,
        key: str,
        key_pool: list[str],
        config: dict,
    ) -> None:
        if op_name == "GET":
            client.get(key)
        elif op_name == "SET":
            client.set(key, self.random_value(rng, config["value_size_min"], config["value_size_max"]))
        elif op_name == "EXISTS":
            client.exists(key)
        elif op_name == "DEL":
            client.delete(key)
            client.set(key, self.random_value(rng, config["value_size_min"], config["value_size_max"]))
        elif op_name == "UNLINK":
            client.unlink(key)
            client.set(key, self.random_value(rng, config["value_size_min"], config["value_size_max"]))
        elif op_name == "EXPIRE":
            client.expire(key, rng.randint(1, 60))
        elif op_name == "PEXPIRE":
            client.pexpire(key, rng.randint(1000, 60000))
        elif op_name == "RENAME":
            dst = f"{key}:renamed:{rng.randint(1, 100000)}"
            try:
                client.rename(key, dst)
                client.rename(dst, key)
            except redis.ResponseError:
                pass
        elif op_name == "RENAMENX":
            dst = f"{key}:renamenx:{rng.randint(1, 100000)}"
            try:
                client.renamenx(key, dst)
                if client.exists(dst):
                    client.rename(dst, key)
            except redis.ResponseError:
                pass
        elif op_name == "TOUCH":
            client.touch(key)
        elif op_name == "RANDOMKEY":
            client.randomkey()
        else:
            raise ValueError(f"unsupported operation {op_name}")

    def random_value(self, rng: random.Random, minimum: int, maximum: int) -> bytes:
        size = rng.randint(minimum, maximum)
        payload = "".join(rng.choices(string.ascii_letters + string.digits, k=size))
        return payload.encode("utf-8")

    def run_profile_c(self) -> dict:
        self.client.flushall()
        key_count = env_int("STRESS_KEY_COUNT", 1_000_000)
        batch_size = env_int("STRESS_BATCH_SIZE", 1000)
        sizes = [64, 256]
        phases = []

        for value_size in sizes:
            self.client.flushall()
            started = time.perf_counter()
            self.bulk_load_dataset(key_count, value_size, batch_size)
            elapsed = max(0.001, time.perf_counter() - started)
            phases.append(
                {
                    "value_size_bytes": value_size,
                    "key_count": key_count,
                    "batch_size": batch_size,
                    "estimated_payload_bytes": key_count * value_size,
                    "elapsed_secs": round(elapsed, 3),
                    "ops_per_sec": round(key_count / elapsed, 2),
                    "dbsize": self.client.dbsize(),
                    "info_memory": self.safe_info_section("memory"),
                }
            )

        return {
            "mode": "bulk-load",
            "config": {
                "key_count": key_count,
                "batch_size": batch_size,
            },
            "phases": phases,
        }

    def bulk_load_dataset(self, key_count: int, value_size: int, batch_size: int) -> None:
        value = (b"x" * value_size)
        client = self.client
        pipeline = client.pipeline(transaction=False)
        for index in range(key_count):
            pipeline.set(f"stress:C:{value_size}:{index}", value)
            if (index + 1) % batch_size == 0:
                pipeline.execute()
        pipeline.execute()

    def safe_info_section(self, section: str) -> dict:
        try:
            return self.client.info(section)
        except Exception as exc:  # noqa: BLE001
            return {"error": str(exc)}


if __name__ == "__main__":
    raise SystemExit(StressHarness().run())
