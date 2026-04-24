# lambda-infra

Reference IaC for the five DynamoDB tables the `turul-a2a` DynamoDB backend
expects in a Lambda deployment. **Not a Rust crate** — just CloudFormation,
Terraform, and an `aws` CLI script you can copy into your own deployment
repo.

The DynamoDB backend (`DynamoDbA2aStorage::new`) does **not** auto-create
tables. Unlike the SQLite and Postgres backends (which run `CREATE TABLE
IF NOT EXISTS` on construction), DynamoDB tables are a shared AWS
resource owned by your infra pipeline — the framework assumes they exist
and are `ACTIVE` before the first request lands.

## Which tables are needed

All five tables use billing mode `PAY_PER_REQUEST` (on-demand) in the
examples. Switch to provisioned capacity in production if your traffic
is steady enough to justify it. Defaults come from
`DynamoDbConfig::default()` in `crates/turul-a2a/src/storage/dynamodb.rs`.

| Table | Key schema | Stream | TTL attr | Default TTL |
|---|---|---|---|---|
| `a2a_tasks` | `pk` (S) HASH | — | `ttl` | 7 days |
| `a2a_push_configs` | `pk` (S) HASH | — | `ttl` | 7 days |
| `a2a_task_events` | `pk` (S) HASH + `sk` (S) RANGE | — | `ttl` | 24 hours |
| `a2a_push_deliveries` | `pk` (S) HASH + `sk` (S) RANGE | — | `ttl` | 7 days |
| `a2a_push_pending_dispatches` | `pk` (S) HASH + `sk` (S) RANGE | **NEW_IMAGE** | `ttl` | 7 days |

Notes:

- **No GSIs are required.** The backend uses `Scan` with filter
  expressions for tenant/owner lookups and stale-marker sweeps. If you
  need to scale list queries, add a GSI on `(tenant, owner)` to
  `a2a_tasks` and `a2a_push_configs` — the backend scan paths already
  annotate this as a future optimisation.
- **The stream on `a2a_push_pending_dispatches` is mandatory if push
  delivery is enabled.** `StreamViewType: NEW_IMAGE`. Without it, the
  scheduled worker (`examples/lambda-scheduled-worker`) is the only
  fan-out path and webhook latency is bounded by the sweep interval.
- **TTL must be enabled on the `ttl` attribute for each table.** The
  framework writes the attribute as epoch-seconds (defaults above), but
  DynamoDB does not evict rows until the TTL feature is turned on for
  that attribute. All three IaC variants in this directory turn it on.

## Three equivalent variants

Pick one — they produce the same tables.

### 1. CloudFormation / SAM

```bash
aws cloudformation deploy \
  --stack-name a2a-tables \
  --template-file cloudformation.yaml \
  --no-fail-on-empty-changeset
```

Adds five `AWS::DynamoDB::Table` resources and emits the stream ARN for
`a2a_push_pending_dispatches` as a stack output (feed this into the
`EventSourceMapping` for `lambda-stream-worker`).

### 2. Terraform

```bash
cd terraform
terraform init
terraform apply
```

See `terraform/outputs.tf` for the stream ARN. The variables in
`terraform/variables.tf` let you prefix table names per-environment
(e.g. `staging_a2a_tasks`).

### 3. AWS CLI (dev / local loops)

```bash
./provision.sh              # create all five tables + stream + TTL
./provision.sh --teardown   # delete them again
```

Uses only `aws dynamodb` subcommands — no extra tooling needed. Safe to
re-run: skips `ResourceInUseException` on already-existing tables.

## IAM for the three Lambdas

Minimum permissions, scoped to the five table ARNs (and the stream
ARN for the worker):

**`lambda-agent` (request):**

- `dynamodb:GetItem`, `PutItem`, `UpdateItem`, `DeleteItem`,
  `TransactWriteItems`, `BatchGetItem`, `Scan`, `Query` on all five
  tables.
- No stream permissions (it writes markers; it does not consume them).

**`lambda-stream-worker`:**

- `dynamodb:GetItem`, `Query` on `a2a_tasks`, `a2a_task_events`,
  `a2a_push_configs`.
- `dynamodb:PutItem`, `UpdateItem`, `DeleteItem` on
  `a2a_push_deliveries` and `a2a_push_pending_dispatches`.
- Stream consumer perms on the table stream: `DescribeStream`,
  `GetRecords`, `GetShardIterator`, `ListStreams`.

**`lambda-scheduled-worker`:**

- Same table perms as `lambda-stream-worker`, plus `dynamodb:Scan` on
  `a2a_push_pending_dispatches` (drives the backstop sweep).

The CloudFormation / Terraform templates here provision only the
*tables* — they intentionally do not provision the Lambdas or the IAM
roles, so you can drop them into any deployment shape (SAM, CDK,
serverless.yml, bare `aws lambda create-function`) without
fighting. See the Lambda example READMEs for the execution-role shape.

## Lifecycle

- **Irreversible names.** `DeleteTable` on a populated table is
  instant data loss. All three variants use stable table names; if
  you need blue/green, prefix the names via the environment variable
  (`TABLE_PREFIX=green-`) and migrate the Lambdas across.
- **Stream retention is 24h.** DynamoDB Streams buffer records for
  24 hours. If `lambda-stream-worker` is disabled longer than that,
  records are dropped and the scheduled worker becomes the only
  recovery path (ADR-013 §5).
- **Order matters on first apply.** Create the tables before
  deploying the Lambdas — the request Lambda will fail every write
  until all five tables exist.

## See also

- `crates/turul-a2a/src/storage/dynamodb.rs` — the production
  backend; attribute shapes are authoritative here.
- `examples/lambda-agent` — request Lambda.
- `examples/lambda-durable-agent` + `examples/lambda-durable-worker` — ADR-018 durable executor continuation via SQS (needs these DynamoDB tables in production; their in-memory demo storage doesn't survive the request/worker Lambda handoff).
- `examples/lambda-stream-worker` — stream trigger for push fan-out.
- `examples/lambda-scheduled-worker` — EventBridge backstop.
- ADR-009 (durable event coordination), ADR-011 (push delivery),
  ADR-013 (Lambda push-delivery parity).
