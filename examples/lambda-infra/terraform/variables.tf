variable "table_prefix" {
  description = "Optional prefix for table names (e.g. 'staging-' or 'prod-')."
  type        = string
  default     = ""
}

variable "billing_mode" {
  description = "DynamoDB billing mode. PAY_PER_REQUEST (on-demand) or PROVISIONED."
  type        = string
  default     = "PAY_PER_REQUEST"

  validation {
    condition     = contains(["PAY_PER_REQUEST", "PROVISIONED"], var.billing_mode)
    error_message = "billing_mode must be PAY_PER_REQUEST or PROVISIONED."
  }
}

variable "tags" {
  description = "Tags applied to every table."
  type        = map(string)
  default     = {}
}
