# Launch folder browser and repository setup

Status: folder-action implementation and local validation complete; independent review pending

## Request

Browse for a folder that project discovery did not put in the agent launcher, select it, and keep
it in that machine's project choices for later launches. Navigate throughout the owning machine's
allowed tree, create a folder in the displayed directory, or clone a repository there.

## Acceptance

- The launch dialog provides an accessible, mobile-friendly folder browser.
- Browsing starts at the selected machine's configured project and favorite roots.
- Every requested directory is canonicalized and revalidated by its owning machine; relative,
  outside-root, control-character, symlink, unknown-machine, and offline-machine paths fail closed.
- Listings contain only immediate real child directories and are scan/result bounded.
- Up remains available while the parent is inside any configured root and is disabled only at the
  actual owner-enforced boundary.
- New-folder and clone destinations are bounded single components; traversal, symlink escapes,
  option-like values, and every existing target fail closed.
- Repository clones accept credential-free HTTPS, SSH URLs, or `git@host:path`, execute fixed argv
  as `git clone -- ...` without a shell, disable terminal prompting, redact bounded failures, time
  out, and remove only the incomplete directory created by that request.
- Federated browsing routes through the existing authenticated owner connection, without browser
  access to a remote node or a caller-controlled forwarding hop. Folder mutations use the same
  owner routing, bearer/Host boundary, and mutation-Origin protection.
- Selecting a folder fills the launch form and remembers at most 32 validated absolute folders per
  machine in browser storage. Remembered choices augment, but never bypass, server launch checks.

## Completion gate

- [x] Implemented
- [x] Unit/API/browser tested
- [x] Integration tested
- [ ] Independently reviewed

## Verification

- Rust tests cover root listings, overlapping-root parent navigation, actual-root disabling,
  outside-root rejection, symlink escapes, safe component names, spaces, literal Git argv,
  credential/option/transport rejection, existing targets, failure redaction, and cleanup.
- API tests cover existing authentication and mutation-Origin policy plus exact federated machine
  routing for create and clone requests.
- Web units cover untrusted stored-data normalization, per-machine memory, deduplication, and query
  encoding, safe child names, and repository destination derivation.
- A real headless 390×844 browser navigates up to the allowed root, creates a folder, clones a
  repository, preserves selected-machine routing, verifies 44px controls/16px inputs without
  horizontal overflow, and selects the displayed folder.
