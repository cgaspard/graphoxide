variable "worker_name" {
  type    = string
  default = "matrix"
}

resource "null_resource" "worker" {
  triggers = {
    name = var.worker_name
  }
}

resource "null_resource" "runner" {
  depends_on = [null_resource.worker]
  triggers = {
    worker_id = null_resource.worker.id
  }
}

output "runner_id" {
  value = null_resource.runner.id
}
