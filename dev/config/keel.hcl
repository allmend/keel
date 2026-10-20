# Local dev stack (docker compose): the load balancer.
listener "http" "0.0.0.0:80" {}

access_log {
  enabled = true
  dir     = "/var/log/keel"
}

# Memory-only cache for local dev: no disk path needed in containers.
cache {
  memory = "64M"
}

pool "web" {
  algorithm = "round_robin"

  health_check "http" {
    path                = "/"
    interval            = "5s"
    timeout             = "2s"
    healthy_threshold   = 1
    unhealthy_threshold = 2
  }

  backend "172.29.100.11:80" {}
  backend "172.29.100.12:80" {}
  backend "172.29.100.13:80" {}
}

vhost "*" {
  pool = "web"

  cache {
    enabled = true
    ttl     = "30s"
  }
}
