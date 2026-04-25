#!/usr/bin/env bash
# Smoke test against a running lambda-agent (HTTP-only Function URL
# OR `cargo lambda watch -p lambda-agent` listening on
# http://localhost:9000/lambda-url/turul-a2a-lambda-agent).
#
# Drives one POST /message:send and one GET /tasks/{id}, asserts
# state=COMPLETED and the fixed greeting artifact arrives.
#
# Usage:
#   bash examples/lambda-agent/scripts/smoke.sh <FN_URL>
#   FN_URL=http://localhost:9000/lambda-url/turul-a2a-lambda-agent bash .../smoke.sh

set -euo pipefail

FN_URL="${1:-${FN_URL:-${A2A_FN_URL:-}}}"
if [ -z "$FN_URL" ]; then
  echo "ERROR: pass the Function URL as \$1 or set FN_URL / A2A_FN_URL" >&2
  echo "Example (cargo lambda watch): $0 http://localhost:9000/lambda-url/turul-a2a-lambda-agent" >&2
  echo "Example (deployed):           $0 https://...lambda-url.amazonaws.com" >&2
  exit 2
fi
FN_URL="${FN_URL%/}"
PROBE="lambda-agent-$(date +%s)"

echo "=== agent card ==="
curl -sSf "$FN_URL/.well-known/agent-card.json" \
  | jq '{name, description, skills: [.skills[] | {id, name, tags}]}'

echo
echo "=== POST /message:send (probe=$PROBE) ==="
R=$(mktemp)
curl -sS -X POST "$FN_URL/message:send" \
  -H 'a2a-version: 1.0' -H 'content-type: application/json' \
  -d "{\"message\":{\"messageId\":\"m-$PROBE\",\"role\":\"ROLE_USER\",
        \"parts\":[{\"text\":\"$PROBE\"}]}}" > "$R"
TASK_ID=$(jq -r .task.id "$R")
S0=$(jq -r .task.status.state "$R")
echo "  task_id = $TASK_ID"
echo "  state   = $S0"

echo
echo "=== GET /tasks/\$TASK_ID ==="
T=$(mktemp)
curl -sS "$FN_URL/tasks/$TASK_ID" -H 'a2a-version: 1.0' > "$T"
S=$(jq -r .status.state "$T")
ART=$(jq -r '.artifacts[0].parts[0].text' "$T")
echo "  state    = $S"
echo "  artifact = $ART"

echo
PASS=0; FAIL=0
chk() { if eval "$1"; then echo "OK  $2"; PASS=$((PASS+1)); else echo "FAIL $2"; FAIL=$((FAIL+1)); fi; }
chk "[ \"$S\" = TASK_STATE_COMPLETED ]"  "terminal state COMPLETED"
chk "[ -n \"$TASK_ID\" ]"                "task_id assigned"
chk "[ \"$ART\" != null ]"               "artifact present"
echo
echo "Result: $PASS pass / $FAIL fail"
rm -f "$R" "$T"
exit $FAIL
