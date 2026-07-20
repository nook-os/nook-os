#!/usr/bin/env bash
# Rust owns the types: emit OpenAPI from nook-control, generate TS for the frontend.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo run -q -p nook-openapi-gen > frontend/packages/api/openapi.json
pnpm --dir frontend exec openapi-typescript packages/api/openapi.json -o packages/api/src/generated/schema.d.ts
echo "Generated frontend/packages/api/src/generated/schema.d.ts"
