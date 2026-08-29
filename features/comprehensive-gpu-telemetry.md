# Comprehensive cross-platform GPU telemetry

Status: implementation active

## Acceptance criteria

- Every machine reports every safely available GPU device and stable identity data.
- Linux NVIDIA/AMD/Intel collectors report utilization, memory used/total, temperature, power,
  clocks, fan/thermal data, and driver/runtime metadata when available.
- Apple Silicon and Intel Macs report the GPU, memory/pressure, utilization, power/thermal state,
  temperature, and fan information exposed by supported macOS tools/APIs without requiring unsafe
  code or elevated privileges.
- Missing privileges, tools, drivers, or counters produce explicit unavailable diagnostics rather
  than zero or fabricated readings.
- Federation transports bounded structured device data, and the machine view remains compact on
  desktop and mobile.

## Gates

- [x] Linux collector implementation and fixtures
- [x] macOS collector implementation and fixtures
- [x] Bounded federation/API representation
- [x] Browser/mobile rendering
- [ ] Native Tron, Midnight, and Max verification
- [ ] Fable/Claude Max review
- [ ] Independent security review

## Verification evidence

- Rust fixture coverage includes NVIDIA CSV, Linux DRM/sysfs, current Apple Silicon and Intel Mac
  `system_profiler` shapes, bounded command output, `ioreg`, memory pressure, and thermal state.
- Browser coverage renders all optional counters and diagnostics, including unknown used-memory
  values without fabricating zero.
- Tron discovered three AMD DRM devices. Midnight's native collection commands were verified
  read-only against its current Apple M4 Max output and remain comfortably inside the command
  limits. Final source-binary verification on all three nodes remains open.
