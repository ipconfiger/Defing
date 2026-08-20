#!/usr/bin/env bash
# M0 契约校验：proto / openapi / storage schema 三方 lint（design-v3 §8）
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "== 1/3 proto (config.v1.proto) =="
protoc --descriptor_set_out=/dev/null -I "$ROOT" "$ROOT/proto/config.v1.proto"
echo "proto OK"

echo "== 2/3 openapi (api/openapi.v1.yaml) =="
python3 - "$ROOT/api/openapi.v1.yaml" <<'PY'
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
assert str(d.get("openapi", "")).startswith("3."), "openapi version must be 3.x"
paths = d.get("paths") or {}
assert paths, "paths must be non-empty"
for p, item in paths.items():
    assert isinstance(item, dict) and item, f"path {p} has no operations"
    for op in item.values():
        assert "responses" in op, f"operation {p} missing responses"
print(f"openapi OK: {len(paths)} paths")
PY

echo "== 3/3 storage schema (schema/storage.v1.schema.json) =="
python3 - "$ROOT/schema/storage.v1.schema.json" <<'PY'
import sys, json
d = json.load(open(sys.argv[1]))
assert str(d.get("$schema", "")).startswith("https://json-schema.org"), "missing $schema"
defs = d.get("$defs") or {}
need = ["ItemDef","GroupDef","Structure","StructureDraft","Value","Ciphertext",
        "DraftValue","BranchState","DiffEntry","VersionRecord","SharedItem",
        "SharedVersion","AdminSession","ProjectTokenRecord","AuditEntry"]
missing = [n for n in need if n not in defs]
assert not missing, f"missing $defs: {missing}"
print(f"storage schema OK: {len(defs)} $defs")
PY

echo "ALL CONTRACTS OK"
