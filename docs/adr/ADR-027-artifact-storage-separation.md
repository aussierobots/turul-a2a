# ADR-027: Artifact Storage Separation for Task Stores

- **Status:** Accepted
- **Date:** 2026-06-10

## Context

Every task-storage backend persists a task as a single serialized blob (`taskJson` on DynamoDB, `task_json` on SQLite/PostgreSQL, a whole `Task` value in the in-memory map) and rewrites that blob in full on **every** mutation — status transition, message append, artifact append. Artifact bodies live inside the blob.

This is a cost defect on DynamoDB and a structural defect everywhere:

- **DynamoDB bills writes by item size, and transactional writes cost double.** A task carrying a 104 KB result artifact costs ~110+ WCU per status transition inside `TransactWriteItems` — for a write that semantically changes a few hundred bytes. Switching `PutItem` to `UpdateItem` does **not** help: `UpdateItem` consumes capacity for the larger of the item's before/after images regardless of how few attributes change. Live evidence (downstream deployment, ap-southeast-2): 194,116 WRU/24h on the tasks table, 40 KB average item, dominated by one inline artifact rewritten on every `submitted → working → completed` step.
- **Partial status updates are also a correctness dead end.** The task blob is the documented source of truth; reads deserialize the whole task from it. The status (state, timestamp, message) lives inside the blob — a write that bumps only the `statusState` index attribute would serve stale status on every subsequent read.
- **Reads pay too.** `list_tasks` on DynamoDB scans full items even when the filter sets `include_artifacts = false` — trimming happens client-side after the read, so RCU and latency scale with artifact size on every listing. (`get_task` has no artifact-exclusion parameter; it always returns artifacts.)
- **The hot production path is full-task replace.** The executor event sink persists artifacts via `A2aAtomicStore::update_task_with_events` (full `Task` + `ArtifactUpdate` events), not via `append_artifact`. The trait contract additionally allows status, artifacts, and a terminal transition in a single `update_task_with_events` call (parity test AT-004). Any design must make *that* path cheap, not just the convenience append method.

On PostgreSQL the same shape causes TOAST/WAL churn proportional to artifact size per transition; on SQLite it is mostly harmless; in-memory it is irrelevant. The cost driver is DynamoDB, but the storage *contract* is shared, so the fix is specified once at the contract level and implemented per backend.

## Decision

### 1. Contract and persisted shape: artifact bodies leave the task record

Artifact bodies are persisted as **separate artifact records**, one per `artifact_id`, owned by the same backend (ADR-009 same-backend requirement unchanged).

The persisted task record gains a sibling field; the blob is **not** repurposed:

- The task blob (`taskJson` / `task_json`) remains a **pure serialized proto `Task`** — reads continue to deserialize it directly, exactly as today. Under the new layout its `artifacts` list is simply empty.
- The **artifact manifest** — the list of `(artifact_id, content_fingerprint)` pairs — is a **separate attribute/column** next to the blob (`artifactManifest` on the DynamoDB item, `artifact_manifest` on the SQL row), in the same position as existing sibling metadata like `statusState`. It is internal storage state and never appears in the wire model.
- **Layout discriminator:** manifest attribute/column **absent/NULL** ⇒ legacy record (artifacts inline in the blob, if any). Manifest **present** (even as an empty list) ⇒ new layout (blob artifacts empty; bodies in artifact records). No version sniffing inside the blob, no envelope wrapper — a wrapped `StoredTask` envelope was considered and rejected because it would change the meaning of the existing blob and force shape-sniffing on every legacy read.

Observable behavior is unchanged:

- `get_task(..)` returns a `Task` whose artifacts are rehydrated from artifact records — byte-identical wire shape to today. (`get_task` takes only `history_length`; it has no artifact-exclusion parameter and always rehydrates.)
- `list_tasks` with `TaskFilter.include_artifacts = false` (or unset) returns the same trimmed shape as today, but no longer pays to read artifact bodies at all — artifact exclusion is a list-only capability, exactly as in the current trait surface.
- No `A2aTaskStorage` / `A2aAtomicStore` / `A2aEventStore` trait signature changes. This is a storage-layout change behind the existing contract.

### 2. Reconciliation semantics for full-task writes (including creation)

All paths that accept a full `Task` — `create_task`, `create_task_with_events`, `update_task`, `update_task_with_events` — treat the incoming artifact set as **authoritative** (replace semantics, matching today's behavior):

- **Creation** extracts artifact records and writes the manifest atomically with the task record — a task born with artifacts never has inline bodies under the new layout.
- **Update**: for each incoming artifact, write an artifact record only when the id is new or the fingerprint differs from the manifest. Artifacts present in the manifest but absent from the incoming set are deleted. The manifest is updated in the same atomic write.
- **Fingerprint:** SHA-256 over an **explicitly canonicalized proto JSON** form: convert the artifact's pbjson serde representation to `serde_json::Value`, recursively rewrite every JSON object with its keys in sorted order, serialize that canonical value, and hash the bytes. Determinism is a property of the canonicalizer, not of dependency behavior — it holds regardless of whether `serde_json`'s `preserve_order` feature ever enters the graph. Prost binary encoding is explicitly rejected: `pbjson_types::Struct.fields` is a `HashMap`, so binary map encoding order is nondeterministic. Two unit tests pin the contract: (1) fingerprint stability across an encode/decode round-trip, and (2) two semantically equal artifacts whose `Struct` maps are built with different insertion orders fingerprint identically. If determinism is ever violated the failure mode is a spurious artifact rewrite (cost, not correctness).

The manifest makes diffing free: no pre-read of artifact bodies, no reliance on inspecting the accompanying event vector. An unchanged 104 KB artifact costs zero artifact-record writes on subsequent task updates.

### 3. Status transitions never write artifact bodies (new-layout tasks)

For any task under the new layout (manifest present), `update_task_status` and `update_task_status_with_events` construct the new task record from the stored one with only the status replaced; the task record contains no artifact bodies **by construction**, so the status paths cannot rewrite them. No special-casing, no partial-update machinery. This removes the dominant write-amplification term for every task created or fully rewritten after the upgrade.

**Legacy records are the explicit exception** (see §9): a legacy task's status transition copies its inline artifacts forward unchanged and stays write-amplified until the task is fully rewritten or reaped. Migrating inline artifacts during a status transition was considered and rejected: it would put large artifact-record writes (and the transaction-size cap) into the hot status path to serve a transient population — terminal legacy tasks never receive another status write, and live-at-upgrade tasks drain within their task TTL (24 h default on DynamoDB).

The internal validation read on these paths needs only the stored status and manifest; implementations MAY skip rehydrating artifact bodies for validation. The **returned** `Task` keeps `get_task` parity (artifacts included), so callers see no change.

### 4. `append_artifact` chunk semantics

`append_artifact(append = true)` becomes a read-modify-write of the **single artifact record** (extend `parts`, recompute fingerprint, update manifest + record atomically). A chunked artifact still rewrites its own record per chunk, but no longer drags the whole task with it. Per-chunk sub-records (write-only chunk append, assemble on read) are explicitly deferred; trigger: chunk-heavy streaming workloads where per-chunk artifact rewrite measurably dominates write cost.

`last_chunk` remains transport metadata, not persisted (unchanged, see ADR-006).

### 5. Ordering contract: artifact writes still touch the task record

`list_tasks` orders by `updated_at DESC` with a `updated_at|task_id` pagination cursor (spec-compliance surface). Any artifact mutation MUST therefore still bump the task record's `updated_at` (and manifest), even though it no longer rewrites a blob containing bodies. The task record write stays — it is just small now.

### 6. Isolation and atomicity

- **Owner/tenant scoping** is enforced on every artifact write exactly as on task writes. DynamoDB: the artifact `Put`/`Delete` rides in the same `TransactWriteItems` as the task-record write, whose existing owner `ConditionExpression` guards the whole transaction. SQL: same transaction, owner-scoped `WHERE` on the task row. Parity test `test_owner_isolation_mutations` continues to pin this.
- **Single-terminal-writer CAS** (terminal `ConditionExpression` on the task record) is unchanged; artifact records never carry status, so they cannot bypass it.
- **DynamoDB transaction cap:** `TransactWriteItems` allows 100 items. A single create or update touching more than ~90 artifacts (after task record + event items) is rejected with a clear `A2aStorageError` rather than silently split — replace semantics across a transaction boundary would not be atomic. This bound is documented on the backend.

### 7. Event records keep their artifact payloads

`ArtifactUpdate` stream events continue to embed the full artifact payload in the durable event store. This is **not** an open question: cross-instance `:subscribe` replay rehydrates events from that store (ADR-009); stripping payloads would break streaming on every multi-instance topology. The artifact body is therefore written once to the events table per emission — acceptable, because it is written once, not once per subsequent status transition. Event TTL reaping is unchanged (ADR-026).

### 8. Per-backend layout

Contract uniformity is required; **layout uniformity is not.**

- **DynamoDB:** artifact records live in the existing events table under a distinct partition-key namespace — `pk = {task_key}#artifacts`, `sk = {artifact_id}` — with the **task** TTL written to the per-item `ttl` attribute. A distinct `pk` keeps artifact items out of event queries entirely (event replay uses `sk > :seq` range conditions under the task's event `pk`; sharing that partition with marker sort keys would corrupt replay). This adds **zero new tables, zero IAM changes, zero bootstrap changes** for existing deployments (ADR-019 unaffected). *Recorded alternative:* a dedicated artifacts table — cleaner naming, but new infrastructure on every existing deployment; adopt only if the namespace-sharing proves operationally confusing.
  - **Namespace collision-freedom by key alphabet, not by id restriction.** Every existing record kind in the events table derives its `pk` from `task_key = {tenant}#{task_id}` and therefore contains at least one literal `#` (push markers/configs/deliveries live in their own tables). Artifact `pk`s use an encoding that contains **no raw `#`**: `art|{enc(tenant)}|{enc(task_id)}`, where `enc` percent-escapes `%`, `|`, and `#` in each component. The two key sets are disjoint by construction for *every* possible tenant/task-id string, and the escaping is injective so artifact keys cannot collide with each other. No task-id validation, no narrowing of the protocol id space, no re-encoding of existing task/event keys. (An earlier draft rejected ids containing `#` at the DynamoDB backend; rejected as backend-specific semantics on a protocol-level string.)
  - **List rehydration:** `list_tasks` with `include_artifacts = true` rehydrates from manifest keys via `BatchGetItem` per page (full keys are known from each task's manifest). Cost scales with the artifact bytes the caller asked for — inherent to the request, and strictly no worse than today's scan of inline bodies. With `include_artifacts = false` (and unset), listing never touches artifact records at all.
  - **Deletion:** `delete_task` deletes the artifact partition's items alongside the task item: one `Query` for keys, then looped `BatchWriteItem` deletes (25 per batch, retrying unprocessed items), **artifacts first, task item last**. Any artifact-cleanup failure surfaces as an `Err` — never a silent partial success behind the `bool` return — leaving the task item intact so the call is safely retryable. The only state an interrupted delete can produce is a manifest entry with no record; rehydration treats that as an absent artifact (degraded read of a task that was being deleted, logged). Leaving artifact items to TTL — the current precedent for event items — was rejected because `task_ttl_seconds = 0` (no expiry) would orphan them forever; events keep their existing TTL-reaping behavior unchanged.
- **SQLite / PostgreSQL:** a new `a2a_task_artifacts` table (`tenant`, `task_id`, `artifact_id`, `artifact_json`, `fingerprint`, timestamps; PK `(tenant, task_id, artifact_id)`), created by the existing startup `CREATE TABLE IF NOT EXISTS` migration path — zero operator action. Both SQL backends move together; they are twin implementations and must not drift. `delete_task` and `cleanup_expired()` (ADR-026) delete artifact rows in the same transaction/statement batch as the task rows.
- **In-memory:** layout unchanged (whole-`Task` map). It has no write-amplification to fix; restructuring it would be speculative complexity. It satisfies the contract trivially and runs all parity scenarios.

### 9. Backward compatibility and migration

No offline migration. Read and write rules:

- **Read (rehydration):** artifacts = artifact records ∪ inline-blob artifacts, deduplicated by `artifact_id`, **records win**. A legacy task (artifacts inline, no manifest) reads identically to today.
- **Write-through migration:** a full-task write under the new code persists artifact records and strips inline bodies **in the same atomic write**. Stripping inline bodies in any write that does not also persist the records is forbidden — a crash between the two would lose artifacts.
- A legacy task that only ever receives status transitions keeps its inline artifacts indefinitely: status paths copy the stored blob's artifact section forward unchanged. Such tasks **remain write-amplified** until a full-task write migrates them or retention reaps them — the deliberate trade pinned in §3, correct and safe, never lossy.

Rollback note: a deployment rolled back to a pre-ADR-027 release reads tasks whose blobs lack artifact bodies and would return artifact-less tasks. Operators pinning task TTLs measured in hours (the DynamoDB default is 24 h) carry low exposure; the CHANGELOG entry must state the rollback caveat explicitly.

### 10. Object-store offload is deferred

S3 (or any object-store) offload is **not** part of this change: it adds an SDK dependency, bucket/IAM/lifecycle configuration, cross-store consistency and failure modes, and benefits only one backend. Separate records already cut the repeated-write cost and keep all four backends symmetric. **Trigger to revisit:** artifact bodies approaching the DynamoDB 400 KB item limit, or operator evidence that single-write artifact cost (not rewrite cost) dominates.

### 11. Test plan

Scenario/parity first (`parity_tests.rs`, every function takes trait objects and runs on all four backends):

1. Large artifact + multiple status transitions → `get_task(include_artifacts)` returns the identical `Task`.
2. Same task read without artifacts → trimmed shape unchanged.
3. Chunked `append_artifact` then status transition → chunks intact, order preserved.
4. `update_task_with_events` carrying status + new artifact + terminal in one call (extends AT-004) → artifacts and events both durable.
5. Artifact replace/removal via full-task write → removed artifact absent on read.
6. Legacy inline-blob task (seeded through a backend test hook) → reads correctly; status transition preserves inline artifacts; first full-task write migrates it.
7. Artifact mutation bumps `updated_at` → `list_tasks` ordering and cursor behavior unchanged.
8. `list_tasks` with `include_artifacts = true` → every page entry rehydrates full artifact bodies; with `include_artifacts = false`/unset → trimmed shape unchanged (both branches of `TaskFilter.include_artifacts`).
9. Fingerprint determinism: encode/decode round-trip yields the identical fingerprint, and semantically equal artifacts built with different map insertion orders fingerprint identically (no spurious rewrites).
10. `create_task` with artifacts already present → reads back identically; subsequent status transition leaves artifact records untouched (backend shape assertion below).
11. `delete_task` removes artifact records along with the task (verified via the backend's own read paths).

Then backend-specific **shape** assertions (not consumed-capacity numbers, which are flaky in CI):

- DynamoDB (env-gated live test): after a status transition, the task item contains no artifact bodies and the artifact item is byte-identical (unchanged timestamps/fingerprint).
- SQLite/PostgreSQL: after a status transition, the artifact row is unmodified.
- Consumed-capacity / item-size before-after measurement stays an operator probe in the downstream deployment, not a CI assertion.

### 12. Versioning and CHANGELOG

Runtime-behavior and storage-layout change with operator-facing notes (new SQL table auto-created; DynamoDB artifact partitions in the events table; rollback caveat). The repo convention is "patch = compatible runtime, minor = contract change". The wire contract is unchanged, but the **persisted storage layout** changes in a way an operator can observe on rollback — if the persisted layout is treated as part of the operator contract (the rollback caveat suggests it is), minor would be the natural classification. **The maintainer has explicitly classified this release as a patch bump** (compatible runtime: wire contract unchanged, reads remain backward compatible, no adopter code changes), accepting the rollback caveat as a documented operational note rather than a contract break. The CHANGELOG entry must state: what changed for operators (write-cost reduction, new artifact records), that the wire contract is unchanged, the rollback caveat, and a `### Verification` line for the parity suite + live-backend gates.

## Consequences

- **Write cost:** a status transition on a task with a 104 KB artifact drops from ~110+ WCU (transactional full-item rewrite) to a few WCU (small task record + event). Comfortably exceeds the ≥ 50 % reduction target; artifact bodies are written once per content change instead of once per lifecycle step.
- **Read cost:** `list_tasks` scans and artifact-excluded reads stop paying for artifact bodies entirely on DynamoDB.
- **Complexity moved, not grown:** reconciliation (manifest diff) replaces blind blob rewrite; the manifest is the single new concept. Trait surfaces, wire shapes, and the state machine are untouched.
- **All four backends keep parity** through shared scenarios; in-memory proves the contract is layout-agnostic.
- **Lambda topology unaffected:** same traits, same same-backend requirement; no new infrastructure for existing deployments.
- **Known residual costs:** chunk-append still rewrites one artifact record per chunk (deferred trigger recorded); event records still carry one full artifact copy each (required by streaming replay); message history remains inside the task blob — a history-heavy workload would motivate a sibling separation, recorded here as a future trigger, not designed speculatively.

## Cross-references

- ADR-003 — storage trait design (tenant/owner scoping the artifact records inherit).
- ADR-006 — `last_chunk` transport semantics (unchanged).
- ADR-009 — durable event coordination (same-backend requirement; event replay constraint pinning §7).
- ADR-019 — least-privilege storage bootstrap (no new DynamoDB infrastructure under §8).
- ADR-026 — retention/TTL (artifact records adopt task TTL; cleanup deletes them with their task).
