# lambda-scheduled-worker

The **mandatory backstop worker** in an ADR-013 Lambda deployment. Triggered
on a cron schedule by EventBridge Scheduler; each invocation runs one sweep
of stale pending-dispatch markers plus expired-non-terminal claims.

Part of a three-Lambda deployment:

1. `lambda-agent` — request Lambda.
2. `lambda-stream-worker` — DynamoDB Stream fast path.
3. `lambda-scheduled-worker` *(this crate)* — mandatory backstop.

## Why this Lambda is mandatory

- For **non-DynamoDB backends** (SQLite, Postgres, in-memory) there is no
  event stream — this scheduled worker is the *only* recovery path.
- For **DynamoDB** it runs alongside the stream worker as belt-and-braces:
  the stream has 24 h retention and can be stalled by a poison record; the
  scheduler picks up anything the stream missed.

See ADR-013 §5.3.

## What one tick does

Each invocation, `LambdaScheduledRecoveryHandler::handle_scheduled_event`:

1. Calls `list_stale_pending_dispatches` (up to `stale_markers_limit`) and
   redispatches each via `PushDispatcher::try_redispatch_pending`.
2. Calls `list_reclaimable_claims` (up to `reclaimable_claims_limit`) for
   claims that expired mid-delivery and redispatches each.

Returns a `LambdaScheduledRecoveryResponse` summary — the example also emits
it as a structured `tracing::info!` line so CloudWatch metric filters can
pick up per-tick counts.

## Build

```bash
cargo lambda build --release -p lambda-scheduled-worker
# Output: target/lambda/lambda-scheduled-worker/bootstrap.zip
```

## Deploy

1. Create an EventBridge Scheduler schedule targeting this Lambda.
   Recommended cadence: every 5 minutes. The input payload is ignored.
2. Grant the execution role `Scan` / `Query` / `Delete` on the pending
   dispatches + deliveries tables and `Get` on the tasks table.
3. Tune `LambdaScheduledRecoveryConfig` to your workload:
   - `stale_cutoff` — should be ≥ `push_claim_expiry` so the sweep doesn't
     race live claims. The example uses 10 min.
   - `stale_markers_limit` / `reclaimable_claims_limit` — cap batch size so a
     post-outage backlog recovers over several ticks instead of blowing out
     one invocation.

## Storage

As with the stream worker, the example uses `InMemoryA2aStorage` for
`cargo check` convenience; production must share the request Lambda's backend
(ADR-009).

## See also

- `examples/lambda-infra` — reference IaC for the five DynamoDB tables.
- ADR-011 (push delivery), ADR-013 (Lambda push-delivery parity).
- `examples/lambda-stream-worker` — the fast path this worker backs up.
