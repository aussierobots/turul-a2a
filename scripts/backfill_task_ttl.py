#!/usr/bin/env python3
"""Backfill the `ttl` attribute on turul-a2a DynamoDB *task* rows.

Why this exists: a defect in the DynamoDB backend's status-update write
path replaced the whole task item without re-writing `ttl`, `tenant`,
and `taskId`. DynamoDB PutItem is whole-item replacement, so any task
that transitioned state lost those attributes — leaving table TTL
enabled but no `ttl` value to act on. The framework fix (>= 0.1.27)
stops the bleed; this script repairs rows already written.

It is idempotent and safe to re-run: each row is updated under a
`attribute_not_exists(ttl)` condition, so rows that already have a TTL
(or were rewritten by the fixed code) are skipped, not clobbered.

For every row missing `ttl` it sets:
  - ttl   = now + --ttl-days (epoch seconds)
  - tenant, taskId = reconstructed from pk ("<tenant>#<task_id>"),
    but only if those attributes are currently absent.

Usage:
  # dry run (default): report counts, write nothing
  python3 scripts/backfill_task_ttl.py --tables sw-a2a-tasks sv-a2a-tasks

  # apply
  python3 scripts/backfill_task_ttl.py --tables sw-a2a-tasks sv-a2a-tasks --apply

  python3 scripts/backfill_task_ttl.py --tables sw-a2a-tasks --ttl-days 1 --apply
"""

import argparse
import time

import boto3
from botocore.exceptions import ClientError


def backfill_table(client, table: str, ttl_epoch: int, apply: bool) -> None:
    scanned = missing = updated = skipped = 0
    paginator = client.get_paginator("scan")
    # Project only the keys we need to decide and to rebuild tenant/taskId.
    for page in paginator.paginate(
        TableName=table,
        ProjectionExpression="pk, tenant, taskId, #t",
        ExpressionAttributeNames={"#t": "ttl"},
    ):
        for item in page.get("Items", []):
            scanned += 1
            if "ttl" in item:
                continue
            missing += 1
            pk = item["pk"]["S"]
            # pk is "<tenant>#<task_id>"; split on the FIRST '#' only,
            # since task ids may themselves contain '#'.
            tenant, _, task_id = pk.partition("#")

            set_parts = ["#t = :ttl"]
            names = {"#t": "ttl"}
            values = {":ttl": {"N": str(ttl_epoch)}}
            if "tenant" not in item:
                set_parts.append("tenant = :tenant")
                values[":tenant"] = {"S": tenant}
            if "taskId" not in item:
                set_parts.append("taskId = :taskId")
                values[":taskId"] = {"S": task_id}

            if not apply:
                updated += 1
                continue
            try:
                client.update_item(
                    TableName=table,
                    Key={"pk": {"S": pk}},
                    UpdateExpression="SET " + ", ".join(set_parts),
                    ConditionExpression="attribute_not_exists(#t)",
                    ExpressionAttributeNames=names,
                    ExpressionAttributeValues=values,
                )
                updated += 1
            except ClientError as e:
                if e.response["Error"]["Code"] == "ConditionalCheckFailedException":
                    skipped += 1  # raced with a concurrent writer; fine
                else:
                    raise

    mode = "APPLIED" if apply else "DRY-RUN (no writes)"
    print(
        f"[{table}] {mode}: scanned={scanned} missing_ttl={missing} "
        f"updated={updated} skipped_raced={skipped}"
    )


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--tables", nargs="+", required=True, help="task table names")
    ap.add_argument("--ttl-days", type=float, default=1.0, help="TTL horizon (default 1)")
    ap.add_argument("--region", default=None, help="AWS region override")
    ap.add_argument("--apply", action="store_true", help="write changes (default: dry run)")
    args = ap.parse_args()

    client = boto3.client("dynamodb", region_name=args.region)
    ttl_epoch = int(time.time() + args.ttl_days * 86400)
    print(f"TTL horizon: now + {args.ttl_days} day(s) -> epoch {ttl_epoch}")
    for table in args.tables:
        backfill_table(client, table, ttl_epoch, args.apply)


if __name__ == "__main__":
    main()
