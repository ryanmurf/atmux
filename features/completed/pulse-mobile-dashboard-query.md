# Pulse mobile dashboard query compatibility

Status: completed

## Request

Make the authenticated mobile Usage dashboard load the populated Rust Pulse data.

## Resolution

The browser correctly requested bounded pages with `limit=100`, but REST query structs flattened a
nested `PageRequest`. Axum's form deserializer then presented the numeric value as a string to the
nested `usize`, returning HTTP 400 for Usage, Pace, Context, and Alerts. The REST-only query structs
now expose explicit bounded `cursor` and `limit` fields and reconstruct the typed page request after
deserialization.

## Completion gate

- [x] Exact iPhone request URLs reproduced from authenticated proxy logs
- [x] Real-router regression covers all four affected endpoints
- [x] Strict Clippy and formatting clean
- [x] Deployed gateway and local endpoint checks return HTTP 200
