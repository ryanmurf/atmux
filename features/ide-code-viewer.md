# IDE-style Files viewer

Status: implemented and tested locally 2026-09-29; not deployed

## Request

"For files, in the browser can we have code detection and click through like a full IDE? Support
java and other popular languages like rust, typescript."

Before this change the Files view highlighted every file with one generic line regex: no language
detection, no multi-line comments or strings, and no navigation.

## Acceptance criteria

- Source is highlighted per language by a whole-file tokenizer, so block comments, text blocks,
  triple-quoted strings, raw strings, and template literals span lines correctly. The language
  comes from the owner hint, the file name, or a shebang. At least Java, Kotlin, Scala, Groovy,
  Rust, TypeScript/TSX, JavaScript/JSX, Python, Go, C, C++, C#, Swift, Ruby, PHP, shell, SQL, JSON,
  YAML, TOML, XML/HTML, CSS/SCSS, Markdown, Dockerfile, Makefile, Protocol Buffers, and GraphQL.
- A plain tap on a name highlights its occurrences and offers Go to definition and Find references.
  Ctrl/⌘-click and F12 go straight to the definition, and Shift+F12 finds references.
- Import targets open the resolved project file for Java/Kotlin/Scala/Groovy, TS/JS, Rust, Python,
  Go, C/C++, Ruby, PHP, CSS/SCSS, and protobuf.
- Definitions are resolved through imports, then same-file declarations (no request), then an
  owner search. One result opens and flashes its line; several are listed.
- Back and Forward (buttons, Alt+←/→) restore file and scroll. Unsaved edits are confirmed first.
  Line selection, Reference selection, wrap and size controls, and Edit/Save/conflict flows keep
  working.
- The owner routes are read-only and forwarded exactly like Files. Symbols are validated as single
  bounded identifiers before any filesystem work, and paths use the Files validation. Traversal is
  descriptor-relative, never follows symlinks, and skips hidden, sensitive, and generated trees.
  Responses carry only project-relative paths and bounded previews. File, byte, time, and result
  budgets stop every scan and report `truncated`.

## Gates

- [x] Implementation (`src/code_nav.rs`, routing in `src/control.rs` and `src/web.rs`, the viewer
  in `web/app.js`, `web/app.css`)
- [x] Focused Rust and dashboard tests
- [x] Browser integration test in the existing mobile dashboard suite
- [ ] Live runtime test on each owner after rollout
- [ ] Fable/Claude Max review
- [ ] Independent security review

## Verification evidence

- `cargo test --lib code_nav::` passed 10 tests. They cover:
  - identifier and specifier validation;
  - declaration detection for Java (classes, constructors, methods, fields, records, enums,
    annotation types), Rust (fns, structs, impls, traits, macros, consts, modules, lifetimes and raw
    strings), TypeScript (classes, methods, properties, interfaces, types, enums, exported consts and
    functions, with calls ignored), Python (classes, functions, module variables, `self.` attributes)
    and Go (grouped consts and types, structs, receivers);
  - family and nearest-directory ranking;
  - import resolution for Java (including static imports), TypeScript (extensions and `index`),
    Rust (`mod`, `crate::`, crate-root items), Python, Go modules, and C includes;
  - refusal of root escapes, bare packages, symlinked files and directories, hidden, sensitive,
    and generated paths, and `..` or `.git` source paths;
  - the time, byte, and result budgets.
- `web::tests` passed 47 tests. They check that the three routes answer 404 for an unknown pane,
  405 for writes, and 400 for a missing symbol or spec, an invalid symbol, an escaping path, or a
  control character. A sensitive path answers 404 like Files, and an offline remote answers 503.
- `web/code-viewer.test.mjs` adds 13 dashboard tests. They cover:
  - Java text blocks and annotations, Rust raw strings, lifetimes, char literals, attributes, and
    macros, TypeScript template literals and generics, and Python triple quotes and f-strings;
  - exact round-trip for twelve more formats;
  - language detection including shebangs;
  - import extraction for TS/JS, Java, Rust, Python, Go, and C;
  - same-file definitions with shadowing;
  - request builder and result validation;
  - history bounds;
  - a check that the viewer never builds HTML from source.
- The mobile browser suite now runs a navigation scenario. It taps a name and checks the panel and
  highlights. It jumps to a same-file definition with no owner request. It Ctrl-clicks an imported
  name, which resolves through the owner, opens the module, and selects the declaration. It then
  checks Back, and a two-result definition list whose chosen entry opens and flashes.
- Tokenizing a 4,000-line, 256 KiB Rust or JavaScript file takes under 20 ms in Node.

## Notes

- Markdown code blocks in Conversation still call `highlightCode` without a language and keep the
  generic highlighting. Passing the fence language would give them the same per-language colors.
- Definition search is heuristic, not a language server. It does not resolve overloads, types of
  receivers, or external dependencies such as `node_modules`, crates, or JDK classes.
