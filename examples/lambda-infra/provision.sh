#!/usr/bin/env bash
# Provision (or teardown) the five DynamoDB tables the turul-a2a Lambda
# adapter expects. For dev/test loops — for production, prefer
# cloudformation.yaml or the terraform/ module in this directory.
#
# Usage:
#   ./provision.sh              # create all five tables + enable TTL + stream
#   ./provision.sh --teardown   # delete all five tables (destructive)
#
# Respects:
#   TABLE_PREFIX   Optional prefix, e.g. "staging-". Default: "".
#   AWS_REGION     Passed through to aws CLI.
#
# Re-running the create path is safe — ResourceInUseException is swallowed
# for the table itself (TTL / stream enablement is still re-asserted so
# configuration drift is corrected on every run).

set -euo pipefail

PREFIX="${TABLE_PREFIX:-}"

TABLES=(
  "${PREFIX}a2a_tasks"
  "${PREFIX}a2a_push_configs"
  "${PREFIX}a2a_task_events"
  "${PREFIX}a2a_push_deliveries"
  "${PREFIX}a2a_push_pending_dispatches"
)

HASH_ONLY_TABLES=(
  "${PREFIX}a2a_tasks"
  "${PREFIX}a2a_push_configs"
)

HASH_RANGE_TABLES=(
  "${PREFIX}a2a_task_events"
  "${PREFIX}a2a_push_deliveries"
  "${PREFIX}a2a_push_pending_dispatches"
)

STREAM_TABLE="${PREFIX}a2a_push_pending_dispatches"

create_hash_only() {
  local name="$1"
  echo "→ create ${name} (pk HASH)"
  aws dynamodb create-table \
    --table-name "${name}" \
    --attribute-definitions AttributeName=pk,AttributeType=S \
    --key-schema AttributeName=pk,KeyType=HASH \
    --billing-mode PAY_PER_REQUEST \
    >/dev/null 2>&1 \
    || echo "  (already exists, skipping create)"
}

create_hash_range() {
  local name="$1"
  local extra_args=("$@")
  unset "extra_args[0]"
  echo "→ create ${name} (pk HASH + sk RANGE)"
  aws dynamodb create-table \
    --table-name "${name}" \
    --attribute-definitions \
        AttributeName=pk,AttributeType=S \
        AttributeName=sk,AttributeType=S \
    --key-schema \
        AttributeName=pk,KeyType=HASH \
        AttributeName=sk,KeyType=RANGE \
    --billing-mode PAY_PER_REQUEST \
    "${extra_args[@]}" \
    >/dev/null 2>&1 \
    || echo "  (already exists, skipping create)"
}

wait_active() {
  local name="$1"
  echo "  waiting for ${name} to be ACTIVE..."
  aws dynamodb wait table-exists --table-name "${name}"
}

enable_ttl() {
  local name="$1"
  # TTL enable is idempotent — re-asserting has no effect if already on.
  aws dynamodb update-time-to-live \
    --table-name "${name}" \
    --time-to-live-specification "Enabled=true, AttributeName=ttl" \
    >/dev/null 2>&1 \
    || echo "  (TTL already enabled on ${name})"
}

enable_stream() {
  local name="$1"
  echo "→ enable DynamoDB Stream (NEW_IMAGE) on ${name}"
  aws dynamodb update-table \
    --table-name "${name}" \
    --stream-specification "StreamEnabled=true, StreamViewType=NEW_IMAGE" \
    >/dev/null 2>&1 \
    || echo "  (stream already enabled on ${name})"
}

teardown() {
  for name in "${TABLES[@]}"; do
    echo "→ delete ${name}"
    aws dynamodb delete-table --table-name "${name}" >/dev/null 2>&1 \
      || echo "  (does not exist, skipping)"
  done
  for name in "${TABLES[@]}"; do
    aws dynamodb wait table-not-exists --table-name "${name}" || true
  done
  echo "done."
}

if [[ "${1:-}" == "--teardown" ]]; then
  read -r -p "Delete all five tables (${TABLES[*]})? [yes/NO] " confirm
  if [[ "${confirm}" == "yes" ]]; then
    teardown
  else
    echo "aborted."
    exit 1
  fi
  exit 0
fi

for name in "${HASH_ONLY_TABLES[@]}"; do
  create_hash_only "${name}"
done

for name in "${HASH_RANGE_TABLES[@]}"; do
  create_hash_range "${name}"
done

for name in "${TABLES[@]}"; do
  wait_active "${name}"
  enable_ttl "${name}"
done

enable_stream "${STREAM_TABLE}"

echo
echo "✓ all five tables provisioned."
echo
echo "Stream ARN for lambda-stream-worker EventSourceMapping:"
aws dynamodb describe-table --table-name "${STREAM_TABLE}" \
  --query 'Table.LatestStreamArn' --output text
