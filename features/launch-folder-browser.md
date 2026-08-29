# Launch folder browser and memory

Status: implementation and live validation complete; independent review pending

## Request

Browse for a folder that project discovery did not put in the agent launcher, select it, and keep
it in that machine's project choices for later launches.

## Acceptance

- The launch dialog provides an accessible, mobile-friendly folder browser.
- Browsing starts at the selected machine's configured project and favorite roots.
- Every requested directory is canonicalized and revalidated by its owning machine; relative,
  outside-root, control-character, symlink, unknown-machine, and offline-machine paths fail closed.
- Listings contain only immediate real child directories and are scan/result bounded.
- Federated browsing routes through the existing authenticated owner connection, without browser
  access to a remote node or a caller-controlled forwarding hop.
- Selecting a folder fills the launch form and remembers at most 32 validated absolute folders per
  machine in browser storage. Remembered choices augment, but never bypass, server launch checks.

## Completion gate

- [x] Implemented
- [x] Unit/browser tested
- [x] Integration tested
- [ ] Independently reviewed

## Verification

- Rust tests cover root listings, child navigation, parent navigation, outside-root rejection, and
  symlink-escape omission.
- API tests cover successful local roots, outside-root rejection, and offline owner handling.
- Web units cover untrusted stored-data normalization, per-machine memory, deduplication, and query
  encoding.
- A real headless 390×844 browser navigates to an undiscovered folder, selects it, and verifies it
  returns in the launcher after reopening.
- Full all-feature Rust tests, strict all-target Clippy, Rust 1.88 all-target check, rustfmt, and the
  66-test browser unit suite pass on 2026-08-10.
