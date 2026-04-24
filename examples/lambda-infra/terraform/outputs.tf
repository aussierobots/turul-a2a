output "tasks_table_name" {
  value = aws_dynamodb_table.tasks.name
}

output "push_configs_table_name" {
  value = aws_dynamodb_table.push_configs.name
}

output "task_events_table_name" {
  value = aws_dynamodb_table.task_events.name
}

output "push_deliveries_table_name" {
  value = aws_dynamodb_table.push_deliveries.name
}

output "push_pending_dispatches_table_name" {
  value = aws_dynamodb_table.push_pending_dispatches.name
}

# Feed this into the EventSourceMapping for lambda-stream-worker.
output "push_pending_dispatches_stream_arn" {
  description = "Stream ARN for a2a_push_pending_dispatches; feeds lambda-stream-worker."
  value       = aws_dynamodb_table.push_pending_dispatches.stream_arn
}
