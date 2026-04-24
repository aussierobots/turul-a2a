terraform {
  required_version = ">= 1.5"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 5.0"
    }
  }
}

# turul-a2a DynamoDB tables for the Lambda adapter. Provisions the five
# tables DynamoDbA2aStorage expects, enables TTL on the `ttl` attribute,
# and turns on the NEW_IMAGE stream on a2a_push_pending_dispatches for
# the stream-worker Lambda.
#
# Outputs expose the stream ARN for the EventSourceMapping.

locals {
  tasks_table_name              = "${var.table_prefix}a2a_tasks"
  push_configs_table_name       = "${var.table_prefix}a2a_push_configs"
  task_events_table_name        = "${var.table_prefix}a2a_task_events"
  push_deliveries_table_name    = "${var.table_prefix}a2a_push_deliveries"
  push_pending_dispatches_name  = "${var.table_prefix}a2a_push_pending_dispatches"
}

resource "aws_dynamodb_table" "tasks" {
  name         = local.tasks_table_name
  billing_mode = var.billing_mode
  hash_key     = "pk"

  attribute {
    name = "pk"
    type = "S"
  }

  ttl {
    attribute_name = "ttl"
    enabled        = true
  }

  point_in_time_recovery {
    enabled = true
  }

  tags = var.tags
}

resource "aws_dynamodb_table" "push_configs" {
  name         = local.push_configs_table_name
  billing_mode = var.billing_mode
  hash_key     = "pk"

  attribute {
    name = "pk"
    type = "S"
  }

  ttl {
    attribute_name = "ttl"
    enabled        = true
  }

  point_in_time_recovery {
    enabled = true
  }

  tags = var.tags
}

resource "aws_dynamodb_table" "task_events" {
  name         = local.task_events_table_name
  billing_mode = var.billing_mode
  hash_key     = "pk"
  range_key    = "sk"

  attribute {
    name = "pk"
    type = "S"
  }

  attribute {
    name = "sk"
    type = "S"
  }

  ttl {
    attribute_name = "ttl"
    enabled        = true
  }

  tags = var.tags
}

resource "aws_dynamodb_table" "push_deliveries" {
  name         = local.push_deliveries_table_name
  billing_mode = var.billing_mode
  hash_key     = "pk"
  range_key    = "sk"

  attribute {
    name = "pk"
    type = "S"
  }

  attribute {
    name = "sk"
    type = "S"
  }

  ttl {
    attribute_name = "ttl"
    enabled        = true
  }

  tags = var.tags
}

# The only table with a DynamoDB Stream. NEW_IMAGE delivers the marker
# payload to the lambda-stream-worker EventSourceMapping.
resource "aws_dynamodb_table" "push_pending_dispatches" {
  name             = local.push_pending_dispatches_name
  billing_mode     = var.billing_mode
  hash_key         = "pk"
  range_key        = "sk"
  stream_enabled   = true
  stream_view_type = "NEW_IMAGE"

  attribute {
    name = "pk"
    type = "S"
  }

  attribute {
    name = "sk"
    type = "S"
  }

  ttl {
    attribute_name = "ttl"
    enabled        = true
  }

  tags = var.tags
}
