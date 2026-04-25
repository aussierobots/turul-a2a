#!/usr/bin/env bash
# Life-of-a-Task verification against a deployed lambda-durable-single.
#
# Drives the spec flow against the real Function URL:
#   - Turn 1: originate task   (returnImmediately=true → SQS → executor)
#   - GET /tasks/{task1.id}    (verify terminal artifact)
#   - Turn 2: refinement task  (SAME contextId, referenceTaskIds=[task1.id])
#   - GET /tasks/{task2.id}    (verify NEW task_id, SAME contextId)
#
# Asserts the spec invariants:
#   - both terminals = COMPLETED
#   - task1.id != task2.id (task immutability across refinement)
#   - task1.contextId == task2.contextId (refinement stays in conversation)
#   - artifacts echo their own task_id and the probe text
#
# Usage:
#   bash examples/lambda-durable-single/scripts/life-of-a-task.sh <FN_URL>
#   FN_URL=https://...lambda-url.amazonaws.com bash .../life-of-a-task.sh

set -euo pipefail

FN_URL="${1:-${FN_URL:-${A2A_FN_URL:-}}}"
if [ -z "$FN_URL" ]; then
  echo "ERROR: pass the Function URL as \$1 or set FN_URL / A2A_FN_URL" >&2
  echo "Usage: $0 <https://...lambda-url.amazonaws.com>" >&2
  exit 2
fi
FN_URL="${FN_URL%/}"
PROBE="lot-$(date +%s)"

echo "=================================================================="
echo "Life-of-a-Task on deployed lambda-durable-single"
echo "FN_URL = $FN_URL"
echo "=================================================================="

echo
echo "--- TURN 1: originate the task ---"
R1=$(mktemp)
curl -sS -X POST "$FN_URL/message:send" \
  -H 'a2a-version: 1.0' -H 'content-type: application/json' \
  -d "{
    \"message\":{\"messageId\":\"u1-$PROBE\",\"role\":\"ROLE_USER\",
      \"parts\":[{\"text\":\"$PROBE\"}],
      \"metadata\":{\"trigger_id\":\"trig-$PROBE\",\"attempt\":1}},
    \"configuration\":{\"returnImmediately\":true}
  }" > "$R1"

TASK1_ID=$(jq -r .task.id "$R1")
CTX_ID=$(jq -r .task.contextId "$R1")
echo "POST returned: state=$(jq -r .task.status.state "$R1")"
echo "  task1.id   = $TASK1_ID"
echo "  contextId  = $CTX_ID"

echo
echo "--- Wait 8s for SQS → worker → terminal commit ---"
sleep 8

echo
echo "--- GET /tasks/\$TASK1_ID ---"
T1=$(mktemp)
curl -sS "$FN_URL/tasks/$TASK1_ID" -H 'a2a-version: 1.0' > "$T1"
S1=$(jq -r .status.state "$T1")
ART1=$(jq -r '.artifacts[0].parts[0].text' "$T1")
ART1_ID=$(jq -r '.artifacts[0].artifactId' "$T1")
echo "  state           = $S1"
echo "  artifactId      = $ART1_ID"
echo "  artifact text   ↓"
echo "$ART1" | sed 's/^/    | /'

echo
echo "--- TURN 2: refinement — SAME contextId, referenceTaskIds=[$TASK1_ID] ---"
R2=$(mktemp)
curl -sS -X POST "$FN_URL/message:send" \
  -H 'a2a-version: 1.0' -H 'content-type: application/json' \
  -d "{
    \"message\":{\"messageId\":\"u2-$PROBE\",\"role\":\"ROLE_USER\",
      \"contextId\":\"$CTX_ID\",
      \"referenceTaskIds\":[\"$TASK1_ID\"],
      \"parts\":[{\"text\":\"refine-$PROBE\"}],
      \"metadata\":{\"trigger_id\":\"trig-refine-$PROBE\",\"attempt\":2}},
    \"configuration\":{\"returnImmediately\":true}
  }" > "$R2"
TASK2_ID=$(jq -r .task.id "$R2")
CTX2_ID=$(jq -r .task.contextId "$R2")
echo "POST returned: state=$(jq -r .task.status.state "$R2")"
echo "  task2.id   = $TASK2_ID  (must differ from task1)"
echo "  contextId  = $CTX2_ID  (must equal turn 1 contextId)"

echo
echo "--- Wait 8s for second SQS → worker round ---"
sleep 8

echo
echo "--- GET /tasks/\$TASK2_ID ---"
T2=$(mktemp)
curl -sS "$FN_URL/tasks/$TASK2_ID" -H 'a2a-version: 1.0' > "$T2"
S2=$(jq -r .status.state "$T2")
ART2=$(jq -r '.artifacts[0].parts[0].text' "$T2")
ART2_ID=$(jq -r '.artifacts[0].artifactId' "$T2")
echo "  state           = $S2"
echo "  artifactId      = $ART2_ID"
echo "  artifact text   ↓"
echo "$ART2" | sed 's/^/    | /'

echo
echo "=================================================================="
echo "task_id progression — Life-of-a-Task spec invariants"
echo "=================================================================="
echo "  contextId stays:   $CTX_ID"
echo "  task1.id (turn1):  $TASK1_ID"
echo "  task2.id (turn2):  $TASK2_ID"
echo

PASS=0
FAIL=0
chk() { if eval "$1"; then echo "OK  $2"; PASS=$((PASS+1)); else echo "FAIL $2"; FAIL=$((FAIL+1)); fi; }

chk "[ \"$S1\" = TASK_STATE_COMPLETED ]"        "turn1 terminal=COMPLETED"
chk "[ \"$S2\" = TASK_STATE_COMPLETED ]"        "turn2 terminal=COMPLETED"
chk "[ \"$TASK1_ID\" != \"$TASK2_ID\" ]"        "task immutability — task_ids differ across refinement"
chk "[ \"$CTX_ID\" = \"$CTX2_ID\" ]"            "contextId stable across turns"
chk "echo \"\$ART1\" | grep -q $PROBE"          "turn1 artifact echoes probe text"
chk "echo \"\$ART1\" | grep -q $TASK1_ID"       "turn1 artifact echoes its own task_id"
chk "echo \"\$ART2\" | grep -q refine-$PROBE"   "turn2 artifact echoes refinement text"
chk "echo \"\$ART2\" | grep -q $TASK2_ID"       "turn2 artifact echoes its own task_id"

echo
echo "Result: $PASS pass / $FAIL fail"
rm -f "$R1" "$T1" "$R2" "$T2"
exit $FAIL
