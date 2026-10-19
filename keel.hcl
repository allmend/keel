# /etc/keel/config/keel.hcl: the load balancer. Listeners, pools, vhosts.
# Every *.hcl below /etc/keel/config/ is loaded after this file, in path
# order (pools/web.hcl, vhosts/example.com.hcl, ...). In a cluster this
# directory is the replicated config: identical on every node. How this node
# is run and reached lives in /etc/keel/node.hcl.

listener "http" "0.0.0.0:80" {}

listener "https" "0.0.0.0:443" {}

# Enable PROXY protocol on a listener when Keel sits behind a load balancer
# or proxy that sends the real client address that way:
# listener "http" "0.0.0.0:80" {
#   proxy_protocol = true
# }

access_log {
  enabled = true
  dir     = "/var/log/keel"
}

# Two-tier HTTP cache: hot objects in memory, full working set on disk.
# Omit either tier to use only the other. Omit the whole block to disable.
cache {
  memory = "256M"

  disk {
    path = "/var/cache/keel"
    size = "10G"
  }
}

pool "web" {
  algorithm = "round_robin"

  health_check "http" {
    path                = "/health"
    interval            = "10s"
    timeout             = "2s"
    healthy_threshold   = 2
    unhealthy_threshold = 3
  }

  backend "10.0.0.1:8080" {}
  backend "10.0.0.2:8080" {}
  backend "10.0.0.3:8080" { weight = 2 }
}

vhost "example.com" {
  pool = "web"

  tls {
    acme = true # certificate from Let's Encrypt; or cert and key files
  }

  # Forward the real client IP to backends. "replace" always overwrites
  # incoming headers (prevents spoofing). Use "append" if Keel itself is
  # behind a trusted upstream proxy, and list its CIDR in trusted_proxies.
  forwarded_headers {
    mode = "replace"
  }

  # Cache rules for this vhost. ttl is the fallback when the origin sends no
  # Cache-Control. statuses defaults to [200]; an empty content_types means
  # no content-type restriction.
  cache {
    enabled  = true
    ttl      = "5m"
    statuses = [200, 301]
    content_types = [
      "text/html",
      "text/css",
      "application/javascript",
      "image/*",
      "font/*",
    ]
  }
}
