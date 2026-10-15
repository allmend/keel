//! Backend weights reach the load balancer: a pool weighted 1:3 splits
//! traffic about 1:3, not evenly.

use std::time::Duration;

use anyhow::Result;
use keel_e2e::{http_get, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.84.0/24
services:
  light:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.84.21 } }
  heavy:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.84.22 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.yaml:/etc/keel/node.yaml:ro", "./config:/etc/keel/config:ro"]
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.84.10 } }
"#;

const NODE: &str = r#"
keel:
  workers: 2
  user: nobody
  group: nogroup
"#;

const CONFIG: &str = r#"
listeners:
  - address: 0.0.0.0:80
pools:
  web:
    algorithm: round_robin
    backends:
      - address: 172.29.84.21:80
        weight: 1
      - address: 172.29.84.22:80
        weight: 3
vhosts:
  - host: "*"
    pool: web
"#;

#[test]
#[ignore = "needs Docker"]
fn a_pool_weighted_one_to_three_splits_traffic_one_to_three() -> Result<()> {
    let stack = Stack::up("weights", COMPOSE, &[("node.yaml", NODE.to_owned()), ("config/keel.yaml", CONFIG.to_owned())])?;
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;

    // whoami echoes the address it answered on.
    const REQUESTS: usize = 400;
    let mut light = 0;
    for _ in 0..REQUESTS {
        let (status, body) = http_get(addr, "e2e.test", "/")?;
        assert_eq!(status, 200);
        if body.contains("IP: 172.29.84.21") {
            light += 1;
        }
    }
    // Expected 100 of 400; an unweighted pool gives about 200.
    assert!((80..=120).contains(&light), "light backend served {light} of {REQUESTS}, expected about 100");
    Ok(())
}
