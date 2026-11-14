# Phase 1 tasks

- [x] Config: `servers[].max_nodes`, multicast ranges, `advertise_ip`, identity IPv4 / `machine_id`
- [x] Design memo: distributed design + cross-host edge TODO
- [x] `VtClock`: slot0 = remote min; `allowed_ns` = min(local boards, slot0) + margin
- [x] `ControlToNode::Start { until_ns }`; drop ceiling writes; node min(allowed, until)
- [x] Multicast module: binary VT + ClusterStop; throttle margin/2 or 100ms (+ First-Fit helpers)
- [ ] Arbiter: publish/receive VT; ClusterStop over multicast; remote table → slot0
- [ ] Placement First-Fit wired into frontend create; capacity tracking; cross-host edge reject
- [ ] Create fanout + compensate; pass topology / boards / multicast
- [ ] Backend: spawn assigned boards only; join multicast when configured
- [ ] Frontend: multi-server create/control; status/log aggregate
- [ ] Example YAML + tests for multi-server path
