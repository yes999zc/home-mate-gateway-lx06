#!/bin/sh
# Wrapper invoking the reproducible A/B slot decision drill.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
exec sh "$ROOT/deploy/dot2d/emos/slot_decision_test.sh" "$@"
