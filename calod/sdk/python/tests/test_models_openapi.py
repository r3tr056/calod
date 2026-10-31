"""Typed models checked against docs/openapi.yaml (drift guard).

If the server contract gains/renames a field, this test fails until
the SDK models follow. Fixtures are built from the schema itself.
"""

import dataclasses
from pathlib import Path

import pytest

yaml = pytest.importorskip("yaml")

from calod.models import Effect, Event, RetentionReport, Run, RunExport

OPENAPI = Path(__file__).resolve().parents[3] / "docs" / "openapi.yaml"


def load_schemas() -> dict:
    return yaml.safe_load(OPENAPI.read_text(encoding="utf-8"))["components"]["schemas"]


def sample_for(schema: dict):
    """Build a minimal valid sample from required+properties."""
    sample = {}
    for name, prop in (schema.get("properties") or {}).items():
        kind = prop.get("type")
        if name == "id" or name.endswith("_id"):
            sample[name] = "11111111-1111-1111-1111-111111111111"
        elif kind == "integer":
            sample[name] = 1
        elif kind == "string":
            sample[name] = "x"
        elif kind == "object":
            sample[name] = {}
        elif kind == "array":
            sample[name] = []
        elif kind == "boolean":
            sample[name] = False
        else:
            sample[name] = "x"
    return sample


def test_model_fields_match_openapi_schemas():
    schemas = load_schemas()
    pairs = [(schemas["Effect"], Effect), (schemas["Run"], Run), (schemas["Event"], Event)]
    for schema, model in pairs:
        props = set((schema.get("properties") or {}).keys())
        fields = {f.name for f in dataclasses.fields(model)}
        assert fields == props, f"{model.__name__}: model {fields} != OpenAPI {props}"
        parsed = model.from_dict(sample_for(schema))
        assert parsed.id


def test_openapi_has_phase1_operations():
    doc = yaml.safe_load(OPENAPI.read_text(encoding="utf-8"))
    paths = doc["paths"]
    assert "/runs/{run_id}/export" in paths
    assert "/admin/retention" in paths


def test_run_export_and_retention_models_parse():
    bundle = {
        "schema": "calod.export/1",
        "exported_at": "2026-10-14T00:00:00Z",
        "run": {
            "id": "11111111-1111-1111-1111-111111111111",
            "external_id": "run-1",
            "agent_name": "agent",
            "status": "completed",
            "version": 3,
            "last_event_sequence": 2,
            "created_at": "2026-10-14T00:00:00Z",
            "updated_at": "2026-10-14T00:01:00Z",
        },
        "effects": [],
        "events": [],
        "checkpoints": [],
        "manifest": {
            "algorithm": "HMAC-SHA256",
            "bundle_sha256": "sha256:" + "0" * 64,
            "signature": "ab" * 32,
            "events_head_hash": None,
            "records": {"effects": 0, "events": 0, "checkpoints": 0},
        },
    }
    export = RunExport.from_dict(bundle)
    assert export.run.external_id == "run-1"
    assert export.manifest.algorithm == "HMAC-SHA256"

    report = RetentionReport.from_dict(
        {"runs_removed": 1, "effects_removed": 2, "events_removed": 9, "cutoff_ms": 5}
    )
    assert (report.runs_removed, report.effects_removed) == (1, 2)
