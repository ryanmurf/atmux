//! Bounded, owner-local source navigation for the dashboard's Files viewer.
//!
//! The browser supplies only an identifier or an import specifier plus the
//! project-relative file it came from. The owning node derives the project
//! root from the live pane exactly as the Files API does, walks it
//! descriptor-relatively without following symlinks, and returns
//! project-relative locations with a bounded one-line preview. Hidden,
//! sensitive, and generated directories are never visited, and every scan
//! stops at fixed file, byte, time, and result budgets.
//!
//! Definition detection is deliberately heuristic: it recognizes declaration
//! shapes per language family (keywords such as `class`, `fn`, `def`, `func`,
//! typed Java/C#/C signatures, Go receivers and grouped declarations). It is a
//! reading aid, not a compiler, and it never executes or evaluates project
//! code.

use std::{
    collections::HashSet,
    ffi::OsStr,
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};

use crate::workspace::{
    WorkspaceError, WorkspaceResult, ensure_visible_path, open_absolute_directory,
    path_for_response, project_root, safe_component, sensitive_component, validate_relative_path,
};

const MAX_SYMBOL_BYTES: usize = 128;
const MAX_SPEC_BYTES: usize = 512;
const MAX_NAV_FILES: usize = 20_000;
const MAX_NAV_DIRECTORIES: usize = 6_000;
const MAX_NAV_DEPTH: usize = 32;
const MAX_NAV_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_NAV_BYTES: u64 = 48 * 1024 * 1024;
const MAX_NAV_FILE_BYTES: u64 = 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const NAV_TIME_BUDGET: Duration = Duration::from_secs(2);
const MAX_NAV_RESULTS: usize = 200;
const MAX_PACKAGE_FILES: usize = 50;
const MAX_PREVIEW_CHARS: usize = 200;

/// Generated, vendored, or dependency trees that an editor's "go to
/// definition" would not search by default and that can dwarf the budget.
const SKIPPED_DIRECTORIES: &[&str] = &[
    "node_modules",
    "bower_components",
    "target",
    "build",
    "dist",
    "out",
    "obj",
    "coverage",
    "vendor",
    "venv",
    "__pycache__",
    "Pods",
    "DerivedData",
];

/// One source location. Paths are project-relative and use `/`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeLocation {
    pub path: String,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column, counted in Unicode scalar values.
    pub column: usize,
    /// `class`, `function`, `method`, `variable`, `reference`, `file`, ...
    pub kind: String,
    /// The trimmed source line, bounded and with control characters removed.
    pub preview: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeNavResponse {
    pub pane_id: String,
    /// `definitions`, `references`, or `resolve`.
    pub operation: String,
    /// The validated symbol or import specifier this answers.
    pub query: String,
    pub results: Vec<CodeLocation>,
    /// A file, byte, time, or result budget stopped the search early.
    pub truncated: bool,
}

impl CodeNavResponse {
    #[must_use]
    pub fn with_pane_id(mut self, pane_id: String) -> Self {
        self.pane_id = pane_id;
        self
    }
}

/// One fixed navigation question. Every field is revalidated by the owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeNavRequest {
    Definitions {
        symbol: String,
        path: Option<String>,
    },
    References {
        symbol: String,
        path: Option<String>,
    },
    Resolve {
        path: String,
        spec: String,
        symbol: Option<String>,
    },
}

impl CodeNavRequest {
    /// The route segment below `/api/v1/panes/{id}/code/`.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        match self {
            Self::Definitions { .. } => "definitions",
            Self::References { .. } => "references",
            Self::Resolve { .. } => "resolve",
        }
    }

    /// The query parameters a coordinator forwards to the owning machine.
    #[must_use]
    pub fn query_pairs(&self) -> Vec<(&'static str, &str)> {
        match self {
            Self::Definitions { symbol, path } | Self::References { symbol, path } => {
                let mut pairs = vec![("symbol", symbol.as_str())];
                if let Some(path) = path {
                    pairs.push(("path", path.as_str()));
                }
                pairs
            }
            Self::Resolve { path, spec, symbol } => {
                let mut pairs = vec![("path", path.as_str()), ("spec", spec.as_str())];
                if let Some(symbol) = symbol {
                    pairs.push(("symbol", symbol.as_str()));
                }
                pairs
            }
        }
    }
}

/// Answers one navigation question for the live pane's project.
///
/// # Errors
///
/// Returns an error for an invalid symbol, specifier, or path, or when the
/// pane's project root is unavailable or outside the configured launch roots.
pub async fn navigate(
    pane_cwd: PathBuf,
    allowed_roots: Vec<PathBuf>,
    request: CodeNavRequest,
) -> WorkspaceResult<CodeNavResponse> {
    // Reject malformed input before any filesystem work.
    validate_request(&request)?;
    tokio::task::spawn_blocking(move || {
        let root = project_root(&pane_cwd, &allowed_roots)?;
        navigate_in_root(&root, &request, Instant::now() + NAV_TIME_BUDGET)
    })
    .await
    .map_err(|_| WorkspaceError::internal("source navigation task failed"))?
}

fn validate_request(request: &CodeNavRequest) -> WorkspaceResult<()> {
    match request {
        CodeNavRequest::Definitions { symbol, path }
        | CodeNavRequest::References { symbol, path } => {
            validate_symbol(symbol)?;
            optional_source_path(path.as_deref())?;
        }
        CodeNavRequest::Resolve { path, spec, symbol } => {
            optional_source_path(Some(path))?.ok_or_else(|| {
                WorkspaceError::invalid("import resolution requires the source file path")
            })?;
            validate_spec(spec)?;
            if let Some(symbol) = symbol {
                validate_symbol(symbol)?;
            }
        }
    }
    Ok(())
}

fn navigate_in_root(
    root: &Path,
    request: &CodeNavRequest,
    deadline: Instant,
) -> WorkspaceResult<CodeNavResponse> {
    validate_request(request)?;
    let root_directory = open_absolute_directory(root)?;
    let mut project = Project::new(root_directory, deadline);
    let (query, results, truncated) = match request {
        CodeNavRequest::Definitions { symbol, path } => {
            let from = optional_source_path(path.as_deref())?;
            let (results, truncated) = project.definitions(symbol, from.as_deref());
            (symbol.clone(), results, truncated)
        }
        CodeNavRequest::References { symbol, path } => {
            let from = optional_source_path(path.as_deref())?;
            let (results, truncated) = project.references(symbol, from.as_deref());
            (symbol.clone(), results, truncated)
        }
        CodeNavRequest::Resolve { path, spec, symbol } => {
            let from = optional_source_path(Some(path))?.ok_or_else(|| {
                WorkspaceError::invalid("import resolution requires the source file path")
            })?;
            let (results, truncated) = project.resolve(&from, spec, symbol.as_deref());
            (spec.clone(), results, truncated)
        }
    };
    Ok(CodeNavResponse {
        pane_id: String::new(),
        operation: request.operation().to_owned(),
        query,
        results,
        truncated,
    })
}

/// A symbol is one bounded source identifier: ASCII letters, digits, `_`,
/// and `$`, not starting with a digit. Anything else is rejected before any
/// filesystem work, so a request can never carry a pattern or a path.
fn validate_symbol(symbol: &str) -> WorkspaceResult<()> {
    let bytes = symbol.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= MAX_SYMBOL_BYTES
        && !bytes[0].is_ascii_digit()
        && bytes.iter().all(|byte| is_identifier_byte(*byte));
    if valid {
        Ok(())
    } else {
        Err(WorkspaceError::invalid(
            "symbol must be one source identifier of at most 128 characters",
        ))
    }
}

fn validate_spec(spec: &str) -> WorkspaceResult<()> {
    if spec.trim().is_empty() || spec.len() > MAX_SPEC_BYTES || spec.chars().any(char::is_control) {
        return Err(WorkspaceError::invalid(
            "import specifier is empty, oversized, or contains control characters",
        ));
    }
    Ok(())
}

fn optional_source_path(path: Option<&str>) -> WorkspaceResult<Option<PathBuf>> {
    let Some(path) = path.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let relative = validate_relative_path(path, false)?;
    ensure_visible_path(&relative)?;
    Ok(Some(relative))
}

const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

// ---------------------------------------------------------------------------
// Languages

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Lang {
    Java,
    Kotlin,
    Scala,
    Groovy,
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
    C,
    Cpp,
    CSharp,
    Swift,
    Ruby,
    Php,
    Shell,
    Sql,
    Proto,
    Graphql,
    Css,
}

/// Files that can reference each other's declarations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Family {
    Jvm,
    Rust,
    Web,
    Python,
    Go,
    Native,
    CSharp,
    Swift,
    Ruby,
    Php,
    Shell,
    Sql,
    Proto,
    Graphql,
    Css,
}

impl Lang {
    fn of(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?.to_ascii_lowercase();
        let extension = Path::new(&name).extension()?.to_str()?.to_owned();
        Some(match extension.as_str() {
            "java" => Self::Java,
            "kt" | "kts" => Self::Kotlin,
            "scala" | "sc" => Self::Scala,
            "groovy" | "gradle" => Self::Groovy,
            "rs" => Self::Rust,
            "ts" | "tsx" | "mts" | "cts" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" | "vue" | "svelte" => Self::JavaScript,
            "py" | "pyi" => Self::Python,
            "go" => Self::Go,
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "m" | "mm" => Self::Cpp,
            "cs" => Self::CSharp,
            "swift" => Self::Swift,
            "rb" | "rake" => Self::Ruby,
            "php" => Self::Php,
            "sh" | "bash" | "zsh" => Self::Shell,
            "sql" => Self::Sql,
            "proto" => Self::Proto,
            "graphql" | "gql" => Self::Graphql,
            "css" | "scss" | "sass" | "less" => Self::Css,
            _ => return None,
        })
    }

    const fn family(self) -> Family {
        match self {
            Self::Java | Self::Kotlin | Self::Scala | Self::Groovy => Family::Jvm,
            Self::Rust => Family::Rust,
            Self::TypeScript | Self::JavaScript => Family::Web,
            Self::Python => Family::Python,
            Self::Go => Family::Go,
            Self::C | Self::Cpp => Family::Native,
            Self::CSharp => Family::CSharp,
            Self::Swift => Family::Swift,
            Self::Ruby => Family::Ruby,
            Self::Php => Family::Php,
            Self::Shell => Family::Shell,
            Self::Sql => Family::Sql,
            Self::Proto => Family::Proto,
            Self::Graphql => Family::Graphql,
            Self::Css => Family::Css,
        }
    }

    const fn line_comment(self) -> &'static [u8] {
        match self {
            Self::Python | Self::Ruby | Self::Shell | Self::Graphql => b"#",
            Self::Sql => b"--",
            Self::Css => b"",
            _ => b"//",
        }
    }

    const fn block_comments(self) -> bool {
        !matches!(
            self,
            Self::Python | Self::Ruby | Self::Shell | Self::Graphql
        )
    }

    const fn triple_quotes(self) -> bool {
        matches!(
            self,
            Self::Python | Self::Java | Self::Kotlin | Self::Scala | Self::Groovy | Self::Swift
        )
    }

    const fn backtick_strings(self) -> bool {
        matches!(self, Self::TypeScript | Self::JavaScript | Self::Go)
    }

    /// Typed C-family signatures put the return type before the name.
    const fn typed_signatures(self) -> bool {
        matches!(
            self,
            Self::Java | Self::Groovy | Self::C | Self::Cpp | Self::CSharp
        )
    }

    /// Class members may be declared without a keyword.
    const fn keywordless_members(self) -> bool {
        matches!(
            self,
            Self::Java
                | Self::Groovy
                | Self::Kotlin
                | Self::TypeScript
                | Self::JavaScript
                | Self::CSharp
                | Self::Cpp
                | Self::Php
        )
    }
}

// ---------------------------------------------------------------------------
// Lexing: comments and literal contents are blanked so matching sees code.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Carry {
    #[default]
    Code,
    BlockComment,
    Triple(u8),
    Backtick,
    /// A Rust string may continue across lines; `hashes` is the raw-string
    /// delimiter count, or `None` for an ordinary escaped string.
    RustString(Option<usize>),
}

/// Returns `line` with comment text and literal contents replaced by spaces,
/// byte for byte, so indices in the result are indices in the original.
fn blank_non_code(line: &str, lang: Lang, carry: &mut Carry) -> Vec<u8> {
    let bytes = line.as_bytes();
    let mut output = bytes.to_vec();
    let comment = lang.line_comment();
    let mut index = 0;
    let blank = |output: &mut Vec<u8>, from: usize, to: usize| {
        for byte in &mut output[from..to.min(bytes.len())] {
            *byte = b' ';
        }
    };
    while index < bytes.len() {
        match *carry {
            Carry::BlockComment => {
                if let Some(close) = find(bytes, b"*/", index) {
                    blank(&mut output, index, close + 2);
                    *carry = Carry::Code;
                    index = close + 2;
                } else {
                    blank(&mut output, index, bytes.len());
                    index = bytes.len();
                }
            }
            Carry::Triple(quote) => {
                if let Some(close) = find(bytes, &[quote, quote, quote], index) {
                    blank(&mut output, index, close);
                    *carry = Carry::Code;
                    index = close + 3;
                } else {
                    blank(&mut output, index, bytes.len());
                    index = bytes.len();
                }
            }
            Carry::Backtick => {
                let end = closing_quote(bytes, index, b'`', lang != Lang::Go);
                blank(&mut output, index, end.unwrap_or(bytes.len()));
                if let Some(end) = end {
                    *carry = Carry::Code;
                    index = end + 1;
                } else {
                    index = bytes.len();
                }
            }
            Carry::RustString(hashes) => {
                let end = match hashes {
                    None => closing_quote(bytes, index, b'"', true),
                    Some(count) => {
                        let mut delimiter = vec![b'"'];
                        delimiter.extend(std::iter::repeat_n(b'#', count));
                        find(bytes, &delimiter, index).map(|end| end + count)
                    }
                };
                blank(&mut output, index, end.unwrap_or(bytes.len()));
                if let Some(end) = end {
                    *carry = Carry::Code;
                    index = end + 1;
                } else {
                    index = bytes.len();
                }
            }
            Carry::Code => {
                let rest = &bytes[index..];
                if !comment.is_empty() && rest.starts_with(comment) {
                    blank(&mut output, index, bytes.len());
                    break;
                }
                if lang.block_comments() && rest.starts_with(b"/*") {
                    *carry = Carry::BlockComment;
                    blank(&mut output, index, index + 2);
                    index += 2;
                    continue;
                }
                let byte = bytes[index];
                if lang.triple_quotes()
                    && (byte == b'"' || (byte == b'\'' && lang == Lang::Python))
                    && rest.starts_with(&[byte, byte, byte])
                {
                    *carry = Carry::Triple(byte);
                    index += 3;
                    continue;
                }
                if lang == Lang::Rust {
                    let boundary = index == 0 || !is_identifier_byte(bytes[index - 1]);
                    if boundary && let Some((hashes, width)) = rust_raw_string_start(rest) {
                        *carry = Carry::RustString(Some(hashes));
                        index += width;
                        continue;
                    }
                    if byte == b'"' {
                        *carry = Carry::RustString(None);
                        index += 1;
                        continue;
                    }
                    if byte == b'\'' {
                        // A char literal is 'x' or an escape; otherwise this
                        // is a lifetime or label and stays code.
                        if let Some(end) = rust_char_literal_end(bytes, index) {
                            blank(&mut output, index + 1, end);
                            index = end + 1;
                        } else {
                            index += 1;
                        }
                        continue;
                    }
                }
                if byte == b'`' && lang.backtick_strings() {
                    *carry = Carry::Backtick;
                    index += 1;
                    continue;
                }
                if byte == b'"' || (byte == b'\'' && lang != Lang::Rust) {
                    let end = closing_quote(bytes, index + 1, byte, true);
                    let stop = end.unwrap_or(bytes.len());
                    blank(&mut output, index + 1, stop);
                    index = stop + 1;
                    continue;
                }
                index += 1;
            }
        }
    }
    // Ordinary quoted strings never continue past a line end except in Rust.
    output
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| offset + from)
}

fn closing_quote(bytes: &[u8], mut index: usize, quote: u8, escapes: bool) -> Option<usize> {
    while index < bytes.len() {
        if escapes && bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == quote {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn rust_raw_string_start(rest: &[u8]) -> Option<(usize, usize)> {
    let offset = usize::from(rest.first() == Some(&b'b'));
    if rest.get(offset) != Some(&b'r') {
        return None;
    }
    let hashes = rest[offset + 1..]
        .iter()
        .take_while(|byte| **byte == b'#')
        .count();
    (rest.get(offset + 1 + hashes) == Some(&b'"')).then_some((hashes, offset + hashes + 2))
}

fn rust_char_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    let next = *bytes.get(start + 1)?;
    if next == b'\\' {
        return closing_quote(bytes, start + 1, b'\'', true).filter(|end| end - start <= 12);
    }
    let width = std::str::from_utf8(&bytes[start + 1..])
        .ok()
        .and_then(|rest| rest.chars().next())
        .map_or(1, char::len_utf8);
    (bytes.get(start + 1 + width) == Some(&b'\'')).then_some(start + 1 + width)
}

/// Every word-bounded occurrence of `symbol` in already-blanked code.
fn occurrences(code: &[u8], symbol: &[u8]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(start) = find(code, symbol, from) {
        let end = start + symbol.len();
        let before = start == 0 || !is_identifier_byte(code[start - 1]);
        let after = end >= code.len() || !is_identifier_byte(code[end]);
        if before && after {
            found.push(start);
        }
        from = start + 1;
    }
    found
}

// ---------------------------------------------------------------------------
// Declaration detection

/// Words that can precede a call or expression but never a declared name.
const NON_DECLARATION_WORDS: &[&str] = &[
    "return",
    "new",
    "throw",
    "throws",
    "else",
    "case",
    "await",
    "yield",
    "typeof",
    "instanceof",
    "in",
    "of",
    "delete",
    "sizeof",
    "not",
    "and",
    "or",
    "if",
    "while",
    "for",
    "switch",
    "catch",
    "when",
    "match",
    "goto",
    "do",
    "extends",
    "implements",
    "import",
    "package",
    "using",
    "include",
    "as",
    "is",
    "assert",
    "echo",
    "print",
    "then",
    "try",
    "finally",
    "default",
    "break",
    "continue",
    "where",
    "with",
    "from",
    "super",
    "this",
    "self",
    "true",
    "false",
    "null",
    "nil",
    "None",
];

/// Leading modifiers that do not change whether a member is declared.
const MODIFIERS: &[&str] = &[
    "public",
    "private",
    "protected",
    "internal",
    "static",
    "async",
    "get",
    "set",
    "readonly",
    "abstract",
    "override",
    "export",
    "default",
    "declare",
    "final",
    "synchronized",
    "native",
    "open",
    "virtual",
    "sealed",
    "partial",
    "unsafe",
    "extern",
    "inline",
    "constexpr",
    "explicit",
    "suspend",
    "operator",
    "infix",
    "lateinit",
    "const",
    "pub",
    "fileprivate",
    "mutating",
];

const CONTROL_STARTS: &[&str] = &[
    "return", "if", "else", "while", "for", "switch", "case", "throw", "new", "await", "yield",
    "do", "catch", "try", "assert", "echo", "print", "elif", "unless", "until", "when", "match",
    "goto", "delete",
];

/// Per-file state that spans lines.
#[derive(Debug, Default)]
struct LineContext {
    carry: Carry,
    /// Go `const (`, `var (`, or `type (` group currently open.
    go_group: Option<&'static str>,
}

/// Classifies an occurrence at byte `start` of `code` (already blanked) as a
/// declaration of `symbol`, returning its kind.
#[allow(clippy::too_many_lines)]
fn declaration_kind(
    lang: Lang,
    code: &[u8],
    start: usize,
    symbol: &str,
    context: &LineContext,
) -> Option<&'static str> {
    let text = std::str::from_utf8(code).ok()?;
    let before = text[..start].trim_end();
    let after_index = start + symbol.len();
    let after = text[after_index..].trim_start();
    let next = after.bytes().next();
    let prev_word = trailing_word(before);
    let prev_char = before.bytes().last();
    let indent = indentation(text);
    let first_word = leading_word(text.trim_start());

    // Go grouped declarations: `const (` ... one name per line.
    if lang == Lang::Go
        && let Some(kind) = context.go_group
        && before.is_empty()
    {
        return Some(kind);
    }

    if let Some(word) = prev_word
        && let Some(kind) = keyword_kind(lang, word, indent)
    {
        // Go names the declared shape after the name: `type T struct`.
        if lang == Lang::Go && kind == "type" {
            if after.starts_with("struct") {
                return Some("struct");
            }
            if after.starts_with("interface") {
                return Some("interface");
            }
        }
        return Some(kind);
    }

    match lang {
        Lang::Rust => {
            if before.ends_with("macro_rules!") {
                return Some("macro");
            }
            // `impl<T> Type<T>` and `impl Trait for Type`.
            let head = before.trim_start();
            if head.starts_with("impl")
                && (prev_word == Some("for") || (head.starts_with("impl<") && head.ends_with('>')))
            {
                return Some("impl");
            }
            // Struct fields: `pub name: Type,`.
            if strip_modifiers(before).is_empty()
                && indent > 0
                && after.starts_with(':')
                && !after.starts_with("::")
                && text.trim_end().ends_with(',')
            {
                return Some("field");
            }
        }
        Lang::Kotlin => {
            // Extension functions: `fun Receiver.name(`.
            if prev_char == Some(b'.')
                && strip_modifiers(before).starts_with("fun ")
                && skip_generics(after).starts_with('(')
            {
                return Some("function");
            }
        }
        Lang::Go => {
            // Method receiver: `func (s *Server) Name(`.
            if prev_char == Some(b')') && text.trim_start().starts_with("func (") {
                return Some("method");
            }
        }
        Lang::Python => {
            if indent == 0 && before.is_empty() && is_assignment(after) {
                return Some("variable");
            }
            if before.ends_with("self.") && is_assignment(after) {
                return Some("attribute");
            }
        }
        Lang::Ruby => {
            if before.trim_end().ends_with("def self.") {
                return Some("method");
            }
            if symbol.as_bytes()[0].is_ascii_uppercase()
                && before.is_empty()
                && is_assignment(after)
            {
                return Some("constant");
            }
        }
        Lang::Shell => {
            if before.is_empty() && after.starts_with("()") {
                return Some("function");
            }
        }
        Lang::C | Lang::Cpp => {
            let trimmed = text.trim_start();
            if trimmed.starts_with('#') && before.trim_end().ends_with("define") {
                return Some("macro");
            }
            if trimmed.starts_with("typedef") && next == Some(b';') {
                return Some("type");
            }
            // Out-of-class definitions: `Type Class::name(` and `Class::Class(`.
            if let Some(qualified) = before.strip_suffix("::")
                && skip_generics(after).starts_with('(')
            {
                let class_start = qualified
                    .bytes()
                    .rposition(|byte| !is_identifier_byte(byte))
                    .map_or(0, |index| index + 1);
                let owner = qualified[..class_start].trim_end();
                let owner_word = trailing_word(owner);
                if (owner.is_empty() && indent == 0)
                    || type_like_prefix(owner, owner_word, owner.bytes().last())
                {
                    return Some("method");
                }
            }
        }
        Lang::Sql => {
            let lower = text.to_ascii_lowercase();
            if lower.contains("create") {
                let word = prev_word.map(str::to_ascii_lowercase);
                let object = match word.as_deref() {
                    Some("exists") => ["table", "view", "function", "procedure", "index", "type"]
                        .into_iter()
                        .find(|object| lower.contains(object)),
                    Some(
                        object @ ("table" | "view" | "function" | "procedure" | "index" | "trigger"
                        | "type" | "sequence" | "schema"),
                    ) => [
                        "table",
                        "view",
                        "function",
                        "procedure",
                        "index",
                        "trigger",
                        "type",
                        "sequence",
                        "schema",
                    ]
                    .into_iter()
                    .find(|candidate| *candidate == object),
                    _ => None,
                };
                if let Some(object) = object {
                    return Some(object);
                }
            }
        }
        _ => {}
    }

    let control_start = first_word.is_some_and(|word| CONTROL_STARTS.contains(&word));
    if control_start {
        return None;
    }
    let after_generics = skip_generics(after);
    let calls = after_generics.starts_with('(');
    let type_like_prefix = type_like_prefix(before, prev_word, prev_char);

    // `ReturnType name(` in typed C-family languages.
    if lang.typed_signatures() && calls && type_like_prefix {
        let open = text.len() - after_generics.len();
        if signature_rest_is_declaration(text, open) {
            return Some("method");
        }
    }

    // Keywordless members: constructors, JS/TS class methods, Kotlin/Java
    // bodies (`name(args) {`), and interface signatures (`name(x): T;`).
    let members_start = strip_modifiers(before).is_empty();
    if lang.keywordless_members() && members_start && calls {
        let open = text.len() - after_generics.len();
        if let Some(close) = matching_paren(text.as_bytes(), open) {
            let rest = text[close + 1..].trim();
            let declared = rest == "{"
                || (rest.starts_with('{') && rest.ends_with('}'))
                || (rest.starts_with(':') && (rest.ends_with('{') || rest.ends_with(';')))
                || (rest.starts_with("throws") && rest.ends_with('{'));
            if declared && (indent > 0 || !before.trim().is_empty()) {
                return Some("method");
            }
        }
    }

    // TypeScript/JavaScript class properties and interface members.
    if matches!(lang, Lang::TypeScript | Lang::JavaScript)
        && members_start
        && indent > 0
        && (after.starts_with(':')
            || after.starts_with("?:")
            || after.starts_with("!:")
            || (after.starts_with('=') && !after.starts_with("==") && !after.starts_with("=>")))
    {
        return Some("property");
    }

    // Fields and typed locals: `Type name;`, `Type name = ...`, `Type a, b;`.
    if lang.typed_signatures() && type_like_prefix && !calls && !inside_parens(before) {
        let assigned = next == Some(b';')
            || next == Some(b',')
            || (after.starts_with('=') && !after.starts_with("=="));
        if assigned {
            return Some(if indent <= 4 { "field" } else { "variable" });
        }
        // C# properties: `public int Count { get; set; }`.
        if lang == Lang::CSharp && next == Some(b'{') {
            return Some("property");
        }
    }
    None
}

/// Declaration keywords per family. Variable-style keywords count only at
/// shallow indentation, so locals inside function bodies are not offered as
/// project-wide definitions.
fn keyword_kind(lang: Lang, word: &str, indent: usize) -> Option<&'static str> {
    let variable_ok = indent <= 4;
    let family = lang.family();
    let kind = match (family, word) {
        (_, "class") | (Family::Swift, "actor") => "class",
        (_, "interface") => "interface",
        (_, "enum") => "enum",
        (Family::Jvm | Family::CSharp, "record") => "record",
        (Family::Rust | Family::Native | Family::CSharp | Family::Swift | Family::Go, "struct") => {
            "struct"
        }
        (Family::Rust | Family::Jvm | Family::Php, "trait") => "trait",
        (Family::Rust | Family::Native, "union") => "union",
        // Go `type (` groups are handled by the line context.
        (Family::Rust | Family::Web | Family::Go | Family::Jvm | Family::Graphql, "type")
        | (Family::Jvm | Family::Swift, "typealias")
        | (Family::Proto, "message" | "service")
        | (Family::Graphql, "input" | "scalar" | "union" | "fragment") => "type",
        (Family::Rust, "fn")
        | (Family::Go | Family::Swift, "func")
        | (Family::Web | Family::Php | Family::Shell, "function")
        | (Family::Jvm, "fun")
        | (Family::Python | Family::Ruby | Family::Jvm, "def")
        | (Family::Proto, "rpc")
        | (Family::Graphql, "query" | "mutation" | "subscription") => "function",
        (Family::Rust, "mod")
        | (Family::Ruby | Family::Web, "module")
        | (Family::Web | Family::Native | Family::CSharp | Family::Php, "namespace") => "module",
        (Family::Jvm, "object") => "object",
        (Family::Swift, "protocol") => "protocol",
        (Family::Rust, "const" | "static") | (Family::Php, "const") => "constant",
        (Family::Rust, "impl") => "impl",
        (Family::Web, "const" | "let" | "var")
        | (Family::Go, "const" | "var")
        | (Family::Jvm | Family::Swift, "val" | "var" | "let")
            if variable_ok =>
        {
            "variable"
        }
        _ => return None,
    };
    Some(kind)
}

/// Parameters (`void run(Foo foo, Bar bar)`) look like fields; an unclosed
/// parenthesis before the name means this is a parameter list.
fn inside_parens(before: &str) -> bool {
    let opens = before.bytes().filter(|byte| *byte == b'(').count();
    let closes = before.bytes().filter(|byte| *byte == b')').count();
    opens > closes
}

fn is_assignment(after: &str) -> bool {
    (after.starts_with('=') && !after.starts_with("=="))
        || (after.starts_with(':') && !after.starts_with("::"))
}

fn trailing_word(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let end = bytes.len();
    let start = bytes
        .iter()
        .rposition(|byte| !is_identifier_byte(*byte))
        .map_or(0, |index| index + 1);
    (start < end).then(|| &text[start..end])
}

fn leading_word(text: &str) -> Option<&str> {
    let end = text
        .bytes()
        .position(|byte| !is_identifier_byte(byte))
        .unwrap_or(text.len());
    (end > 0).then(|| &text[..end])
}

fn indentation(text: &str) -> usize {
    text.bytes()
        .take_while(|byte| *byte == b' ' || *byte == b'\t')
        .map(|byte| if byte == b'\t' { 4 } else { 1 })
        .sum()
}

/// Removes leading declaration modifiers and annotations, leaving whatever
/// precedes the name that is not a modifier.
fn strip_modifiers(before: &str) -> &str {
    let mut rest = before.trim_start();
    loop {
        if let Some(annotation) = rest.strip_prefix('@') {
            // `@Override`, `@Test()`: skip the annotation word and any args.
            let word = leading_word(annotation).map_or(0, str::len);
            let mut tail = &annotation[word..];
            if tail.starts_with('(')
                && let Some(close) = matching_paren(tail.as_bytes(), 0)
            {
                tail = &tail[close + 1..];
            }
            rest = tail.trim_start();
            continue;
        }
        let Some(word) = leading_word(rest) else {
            return rest;
        };
        if !MODIFIERS.contains(&word) {
            return rest;
        }
        rest = rest[word.len()..].trim_start();
        if word == "pub"
            && rest.starts_with('(')
            && let Some(close) = matching_paren(rest.as_bytes(), 0)
        {
            rest = rest[close + 1..].trim_start();
        }
        if let Some(stripped) = rest.strip_prefix('*') {
            rest = stripped.trim_start();
        }
    }
}

fn type_like_prefix(before: &str, prev_word: Option<&str>, prev_char: Option<u8>) -> bool {
    if before.ends_with("->")
        || before.ends_with("=>")
        || before.ends_with("&&")
        || before.ends_with("||")
        || before.ends_with("::")
    {
        return false;
    }
    match prev_word {
        Some(word) => {
            !NON_DECLARATION_WORDS.contains(&word)
                && !MODIFIERS.contains(&word)
                && !word.as_bytes()[0].is_ascii_digit()
        }
        None => matches!(prev_char, Some(b'>' | b']' | b'*' | b'&' | b'?')),
    }
}

fn skip_generics(after: &str) -> &str {
    if !after.starts_with('<') {
        return after;
    }
    let mut depth = 0_usize;
    for (index, byte) in after.bytes().enumerate() {
        match byte {
            b'<' => depth += 1,
            b'>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return after[index + 1..].trim_start();
                }
            }
            b'(' | b')' | b';' | b'{' | b'}' | b'=' => return after,
            _ => {}
        }
    }
    after
}

fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// After a typed `name(` the parameter list must be followed by something a
/// declaration allows: a body, `throws`, `;` (abstract/prototype), a C++
/// qualifier, or nothing (the list continues on the next line).
fn signature_rest_is_declaration(text: &str, open: usize) -> bool {
    let Some(close) = matching_paren(text.as_bytes(), open) else {
        return true;
    };
    let rest = text[close + 1..].trim();
    rest.is_empty()
        || rest.starts_with('{')
        || rest.starts_with("throws")
        || rest.starts_with(';')
        || rest.starts_with("const")
        || rest.starts_with("override")
        || rest.starts_with("noexcept")
        || rest.starts_with("->")
        || rest.starts_with("= default")
        || rest.starts_with("= 0")
        || rest.starts_with("= delete")
        || rest.starts_with("where")
        || rest.starts_with(':')
}

fn update_go_group(code: &str, context: &mut LineContext) {
    let trimmed = code.trim();
    if context.go_group.is_some() {
        if trimmed.starts_with(')') {
            context.go_group = None;
        }
        return;
    }
    context.go_group = match trimmed {
        "const (" => Some("constant"),
        "var (" => Some("variable"),
        "type (" => Some("type"),
        _ => None,
    };
}

/// Definition kinds sort ahead of members, which sort ahead of `impl`s.
fn kind_rank(kind: &str) -> u8 {
    match kind {
        "field" | "property" | "variable" | "attribute" | "constant" => 1,
        "impl" => 2,
        "reference" => 3,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Project traversal

#[derive(Clone, Debug)]
struct WalkedFile {
    relative: PathBuf,
    size: u64,
}

struct Project {
    root: File,
    deadline: Instant,
    walked: Option<(Vec<WalkedFile>, bool)>,
    bytes_read: u64,
}

impl Project {
    const fn new(root: File, deadline: Instant) -> Self {
        Self {
            root,
            deadline,
            walked: None,
            bytes_read: 0,
        }
    }

    fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Every visible regular file, walked once per request and bounded.
    fn files(&mut self) -> (&[WalkedFile], bool) {
        if self.walked.is_none() {
            let mut state = WalkState {
                files: Vec::new(),
                directories: 0,
                truncated: false,
                deadline: self.deadline,
            };
            walk_directory(&self.root, Path::new(""), 0, &mut state);
            self.walked = Some((state.files, state.truncated));
        }
        let (files, truncated) = self.walked.as_ref().expect("walk was just performed");
        (files, *truncated)
    }

    /// Reads one walked file's text under the byte budget.
    fn read_text(&mut self, relative: &Path) -> Option<String> {
        let (file, _) = open_at(&self.root, relative, false)?;
        let size = file.metadata().ok()?.len();
        if size > MAX_NAV_FILE_BYTES || self.bytes_read.saturating_add(size) > MAX_NAV_BYTES {
            return None;
        }
        let mut bytes = Vec::with_capacity(usize::try_from(size).ok()?);
        file.take(MAX_NAV_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        self.bytes_read = self
            .bytes_read
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if bytes.contains(&0) {
            return None;
        }
        String::from_utf8(bytes).ok()
    }

    /// Source files in search order: the requesting file, then the same
    /// language family ordered by directory distance, then by path.
    fn ordered_sources(&mut self, from: Option<&Path>) -> (Vec<PathBuf>, bool) {
        let family = from.and_then(Lang::of).map(Lang::family);
        let from_directory = from.and_then(Path::parent).map(Path::to_path_buf);
        let (files, truncated) = self.files();
        let mut sources = files
            .iter()
            .filter(|file| file.size <= MAX_NAV_FILE_BYTES)
            .filter_map(|file| {
                let lang = Lang::of(&file.relative)?;
                if family.is_some_and(|family| lang.family() != family) {
                    return None;
                }
                let same = from.is_some_and(|from| from == file.relative);
                let distance = from_directory.as_deref().map_or(0, |directory| {
                    directory_distance(directory, file.relative.parent().unwrap_or(Path::new("")))
                });
                Some((!same, distance, file.relative.clone()))
            })
            .collect::<Vec<_>>();
        sources.sort();
        (
            sources.into_iter().map(|(_, _, path)| path).collect(),
            truncated,
        )
    }

    fn scan<F>(
        &mut self,
        symbol: &str,
        from: Option<&Path>,
        mut accept: F,
    ) -> (Vec<(u8, bool, usize, CodeLocation)>, bool)
    where
        F: FnMut(Lang, &[u8], usize, &LineContext) -> Option<&'static str>,
    {
        let (sources, mut truncated) = self.ordered_sources(from);
        let from_directory = from.and_then(Path::parent).map(Path::to_path_buf);
        let mut found = Vec::new();
        for (order, relative) in sources.iter().enumerate() {
            if self.expired() {
                truncated = true;
                break;
            }
            if self.bytes_read >= MAX_NAV_BYTES {
                truncated = true;
                break;
            }
            let Some(lang) = Lang::of(relative) else {
                continue;
            };
            let Some(text) = self.read_text(relative) else {
                continue;
            };
            if !text.contains(symbol) {
                continue;
            }
            let same_file = from.is_some_and(|from| from == relative);
            let distance = from_directory.as_deref().map_or(0, |directory| {
                directory_distance(directory, relative.parent().unwrap_or(Path::new("")))
            });
            let mut context = LineContext::default();
            'lines: for (line_index, line) in text.lines().enumerate() {
                let code = blank_non_code(line, lang, &mut context.carry);
                for start in occurrences(&code, symbol.as_bytes()) {
                    if let Some(kind) = accept(lang, &code, start, &context) {
                        found.push((
                            kind_rank(kind),
                            !same_file,
                            distance.saturating_mul(100_000).saturating_add(order),
                            CodeLocation {
                                path: path_for_response(relative),
                                line: line_index + 1,
                                column: line[..start.min(line.len())].chars().count() + 1,
                                kind: kind.to_owned(),
                                preview: preview(line),
                            },
                        ));
                        if found.len() >= MAX_NAV_RESULTS * 4 {
                            break 'lines;
                        }
                    }
                }
                if lang == Lang::Go {
                    update_go_group(&String::from_utf8_lossy(&code), &mut context);
                }
            }
            if found.len() >= MAX_NAV_RESULTS * 4 {
                truncated = true;
                break;
            }
        }
        (found, truncated)
    }

    fn definitions(&mut self, symbol: &str, from: Option<&Path>) -> (Vec<CodeLocation>, bool) {
        let (mut found, truncated) = self.scan(symbol, from, |lang, code, start, context| {
            declaration_kind(lang, code, start, symbol, context)
        });
        found.sort_by(|left, right| {
            (left.0, left.1, left.2, &left.3.path, left.3.line).cmp(&(
                right.0,
                right.1,
                right.2,
                &right.3.path,
                right.3.line,
            ))
        });
        finish(found, truncated)
    }

    fn references(&mut self, symbol: &str, from: Option<&Path>) -> (Vec<CodeLocation>, bool) {
        let (mut found, truncated) = self.scan(symbol, from, |lang, code, start, context| {
            Some(
                if declaration_kind(lang, code, start, symbol, context).is_some() {
                    "definition"
                } else {
                    "reference"
                },
            )
        });
        // Keep source order within a file; nearer files first.
        found.sort_by(|left, right| {
            (left.1, left.2, &left.3.path, left.3.line, left.3.column).cmp(&(
                right.1,
                right.2,
                &right.3.path,
                right.3.line,
                right.3.column,
            ))
        });
        finish(found, truncated)
    }

    // -----------------------------------------------------------------------
    // Import resolution

    fn resolve(
        &mut self,
        from: &Path,
        spec: &str,
        symbol: Option<&str>,
    ) -> (Vec<CodeLocation>, bool) {
        let Some(lang) = Lang::of(from) else {
            return self.resolve_generic(from, spec.trim(), symbol);
        };
        let spec = spec.trim();
        match lang.family() {
            Family::Web | Family::Css => self.resolve_web(from, spec, symbol, lang),
            Family::Jvm => self.resolve_jvm(from, spec, symbol),
            Family::Rust => self.resolve_rust(from, spec, symbol),
            Family::Python => self.resolve_python(from, spec, symbol),
            Family::Go => self.resolve_go(from, spec, symbol),
            Family::Native => self.resolve_include(from, spec, symbol),
            Family::Ruby => self.resolve_ruby(from, spec, symbol),
            Family::Php => self.resolve_php(from, spec, symbol),
            _ => self.resolve_generic(from, spec, symbol),
        }
    }

    fn exists(&self, relative: &Path) -> bool {
        ensure_visible_path(relative).is_ok() && open_at(&self.root, relative, false).is_some()
    }

    fn exists_directory(&self, relative: &Path) -> bool {
        relative.as_os_str().is_empty()
            || (ensure_visible_path(relative).is_ok()
                && open_at(&self.root, relative, true).is_some())
    }

    /// The first existing candidate becomes the answer, located at the
    /// symbol's declaration when one is given and found.
    fn first_existing(
        &mut self,
        candidates: impl IntoIterator<Item = PathBuf>,
        symbol: Option<&str>,
    ) -> Option<Vec<CodeLocation>> {
        let found = candidates
            .into_iter()
            .find(|candidate| self.exists(candidate))?;
        Some(vec![self.locate_in(&found, symbol)])
    }

    fn locate_in(&mut self, relative: &Path, symbol: Option<&str>) -> CodeLocation {
        let file = CodeLocation {
            path: path_for_response(relative),
            line: 1,
            column: 1,
            kind: "file".to_owned(),
            preview: String::new(),
        };
        let (Some(symbol), Some(lang)) = (symbol, Lang::of(relative)) else {
            return file;
        };
        let Some(text) = self.read_text(relative) else {
            return file;
        };
        let mut context = LineContext::default();
        for (line_index, line) in text.lines().enumerate() {
            let code = blank_non_code(line, lang, &mut context.carry);
            for start in occurrences(&code, symbol.as_bytes()) {
                if let Some(kind) = declaration_kind(lang, &code, start, symbol, &context) {
                    return CodeLocation {
                        path: file.path,
                        line: line_index + 1,
                        column: line[..start.min(line.len())].chars().count() + 1,
                        kind: kind.to_owned(),
                        preview: preview(line),
                    };
                }
            }
            if lang == Lang::Go {
                update_go_group(&String::from_utf8_lossy(&code), &mut context);
            }
        }
        file
    }

    /// Walked files whose project path ends with `suffix`, nearest first.
    fn suffix_matches(&mut self, from: &Path, suffix: &Path) -> (Vec<PathBuf>, bool) {
        let directory = from.parent().unwrap_or(Path::new("")).to_path_buf();
        let (files, truncated) = self.files();
        let mut matches = files
            .iter()
            .filter(|file| file.relative.ends_with(suffix))
            .map(|file| {
                (
                    directory_distance(&directory, file.relative.parent().unwrap_or(Path::new(""))),
                    file.relative.clone(),
                )
            })
            .collect::<Vec<_>>();
        matches.sort();
        (
            matches.into_iter().map(|(_, path)| path).collect(),
            truncated,
        )
    }

    fn resolve_web(
        &mut self,
        from: &Path,
        spec: &str,
        symbol: Option<&str>,
        lang: Lang,
    ) -> (Vec<CodeLocation>, bool) {
        let spec = strip_quotes(spec);
        let directory = from.parent().unwrap_or(Path::new(""));
        let base = if spec.starts_with("./") || spec.starts_with("../") {
            join_relative(directory, spec)
        } else if let Some(rest) = spec.strip_prefix("@/").or_else(|| spec.strip_prefix("~/")) {
            // Common bundler alias for the source directory.
            join_relative(Path::new("src"), rest)
                .filter(|path| self.has_prefix_candidate(path, lang))
                .or_else(|| join_relative(Path::new(""), rest))
        } else if let Some(rest) = spec.strip_prefix('/') {
            join_relative(Path::new(""), rest)
        } else if lang == Lang::Css {
            join_relative(directory, spec)
        } else {
            None
        };
        let Some(base) = base else {
            return (Vec::new(), false);
        };
        let candidates = web_candidates(&base, lang);
        (
            self.first_existing(candidates, symbol).unwrap_or_default(),
            false,
        )
    }

    fn has_prefix_candidate(&self, base: &Path, lang: Lang) -> bool {
        web_candidates(base, lang)
            .iter()
            .any(|candidate| self.exists(candidate))
    }

    fn resolve_jvm(
        &mut self,
        from: &Path,
        spec: &str,
        symbol: Option<&str>,
    ) -> (Vec<CodeLocation>, bool) {
        let spec = spec
            .trim_start_matches("import")
            .trim()
            .trim_start_matches("static")
            .trim()
            .trim_end_matches(';')
            .trim();
        let segments = spec
            .split('.')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if segments.is_empty()
            || segments
                .iter()
                .any(|segment| *segment != "*" && validate_symbol(segment).is_err())
        {
            return (Vec::new(), false);
        }
        let mut truncated = false;
        if segments.last() == Some(&"*") {
            let package = segments[..segments.len() - 1].iter().collect::<PathBuf>();
            let (files, walk_truncated) = self.files();
            let mut members = files
                .iter()
                .filter(|file| {
                    file.relative
                        .parent()
                        .is_some_and(|parent| parent.ends_with(&package))
                        && Lang::of(&file.relative).is_some_and(|lang| lang.family() == Family::Jvm)
                })
                .map(|file| file.relative.clone())
                .collect::<Vec<_>>();
            members.sort();
            truncated |= walk_truncated || members.len() > MAX_PACKAGE_FILES;
            members.truncate(MAX_PACKAGE_FILES);
            let results = members
                .iter()
                .map(|member| self.locate_in(member, symbol))
                .collect();
            return (results, truncated);
        }
        for length in (1..=segments.len()).rev() {
            let class = segments[length - 1];
            let wanted = symbol
                .or_else(|| segments.get(length).copied())
                .unwrap_or(class);
            for extension in ["java", "kt", "scala", "groovy", "kts"] {
                let mut suffix = segments[..length - 1].iter().collect::<PathBuf>();
                suffix.push(format!("{class}.{extension}"));
                let (matches, walk_truncated) = self.suffix_matches(from, &suffix);
                truncated |= walk_truncated;
                if let Some(found) = matches.first() {
                    return (vec![self.locate_in(found, Some(wanted))], truncated);
                }
            }
        }
        // Kotlin top-level functions live in any file of their package.
        if segments.len() >= 2 {
            let name = symbol.unwrap_or(segments[segments.len() - 1]);
            let package = segments[..segments.len() - 1].iter().collect::<PathBuf>();
            let (files, walk_truncated) = self.files();
            let members = files
                .iter()
                .filter(|file| {
                    file.relative
                        .parent()
                        .is_some_and(|parent| parent.ends_with(&package))
                        && Lang::of(&file.relative).is_some_and(|lang| lang.family() == Family::Jvm)
                })
                .map(|file| file.relative.clone())
                .take(MAX_PACKAGE_FILES)
                .collect::<Vec<_>>();
            truncated |= walk_truncated;
            for member in members {
                let location = self.locate_in(&member, Some(name));
                if location.kind != "file" {
                    return (vec![location], truncated);
                }
            }
        }
        (Vec::new(), truncated)
    }

    fn resolve_rust(
        &mut self,
        from: &Path,
        spec: &str,
        symbol: Option<&str>,
    ) -> (Vec<CodeLocation>, bool) {
        let mut spec = spec.trim().trim_end_matches(';').trim();
        for prefix in ["pub(crate) ", "pub(super) ", "pub ", "use "] {
            spec = spec.strip_prefix(prefix).unwrap_or(spec).trim();
        }
        let module_directory = rust_module_directory(from);
        if let Some(name) = spec.strip_prefix("mod ") {
            let name = name.trim();
            if validate_symbol(name).is_err() {
                return (Vec::new(), false);
            }
            let candidates = [
                module_directory.join(format!("{name}.rs")),
                module_directory.join(name).join("mod.rs"),
            ];
            return (
                self.first_existing(candidates, symbol).unwrap_or_default(),
                false,
            );
        }
        let path = spec.split("::{").next().unwrap_or(spec);
        let mut segments = path
            .split("::")
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if segments.is_empty()
            || segments
                .iter()
                .any(|segment| *segment != "*" && validate_symbol(segment).is_err())
        {
            return (Vec::new(), false);
        }
        if segments.last() == Some(&"*") {
            segments.pop();
        }
        let crate_source = self.rust_crate_source(from);
        let mut bases = Vec::new();
        match segments.first().copied() {
            Some("crate") => {
                segments.remove(0);
                bases.push(crate_source.clone());
            }
            Some("self") => {
                segments.remove(0);
                bases.push(module_directory.clone());
            }
            Some("super") => {
                let mut base = module_directory.clone();
                while segments.first() == Some(&"super") {
                    segments.remove(0);
                    base = base.parent().map(Path::to_path_buf).unwrap_or_default();
                }
                bases.push(base);
            }
            Some(first) => {
                // A 2018-edition path may name a local module or a sibling
                // workspace crate; external crates are not resolved.
                bases.push(crate_source.clone());
                bases.push(module_directory.clone());
                let mut truncated = false;
                for name in [first.to_owned(), first.replace('_', "-")] {
                    let suffix = Path::new(&name).join("src").join("lib.rs");
                    let (matches, walk_truncated) = self.suffix_matches(from, &suffix);
                    truncated |= walk_truncated;
                    if let Some(lib) = matches.first()
                        && let Some(source) = lib.parent()
                    {
                        if segments.len() == 1 {
                            return (vec![self.locate_in(lib, symbol)], truncated);
                        }
                        let rest = segments[1..].to_vec();
                        if let Some(found) = self.rust_segments(source, &rest, symbol) {
                            return (found, truncated);
                        }
                    }
                }
            }
            None => {}
        }
        for base in bases {
            if let Some(found) = self.rust_segments(&base, &segments, symbol) {
                return (found, false);
            }
        }
        (Vec::new(), false)
    }

    fn rust_segments(
        &mut self,
        base: &Path,
        segments: &[&str],
        symbol: Option<&str>,
    ) -> Option<Vec<CodeLocation>> {
        if segments.is_empty() {
            let root = ["lib.rs", "main.rs", "mod.rs"]
                .iter()
                .map(|name| base.join(name))
                .find(|candidate| self.exists(candidate))?;
            return Some(vec![self.locate_in(&root, symbol)]);
        }
        for length in (1..=segments.len()).rev() {
            let module = segments[..length].iter().collect::<PathBuf>();
            let item = symbol.or_else(|| segments.get(length).copied());
            let candidates = [
                base.join(&module).with_extension("rs"),
                base.join(&module).join("mod.rs"),
            ];
            if let Some(found) = candidates
                .into_iter()
                .find(|candidate| self.exists(candidate))
            {
                return Some(vec![self.locate_in(&found, item)]);
            }
        }
        // The item may be declared in the crate root itself.
        let root = ["lib.rs", "main.rs", "mod.rs"]
            .iter()
            .map(|name| base.join(name))
            .find(|candidate| self.exists(candidate))?;
        let location = self.locate_in(&root, symbol.or(segments.first().copied()));
        (location.kind != "file").then(|| vec![location])
    }

    fn rust_crate_source(&self, from: &Path) -> PathBuf {
        for ancestor in from.ancestors().skip(1) {
            if self.exists(&ancestor.join("Cargo.toml")) {
                let source = ancestor.join("src");
                return if self.exists_directory(&source) {
                    source
                } else {
                    ancestor.to_path_buf()
                };
            }
        }
        PathBuf::from("src")
    }

    fn resolve_python(
        &mut self,
        from: &Path,
        spec: &str,
        symbol: Option<&str>,
    ) -> (Vec<CodeLocation>, bool) {
        let spec = spec.trim();
        let (module, name) = if let Some(rest) = spec.strip_prefix("from ") {
            let mut parts = rest.splitn(2, " import ");
            let module = parts.next().unwrap_or_default().trim();
            let name = parts
                .next()
                .and_then(|names| names.split(',').next())
                .map(|name| name.trim().trim_start_matches('(').trim())
                .and_then(|name| name.split_whitespace().next());
            (module, name)
        } else {
            let rest = spec.strip_prefix("import ").unwrap_or(spec);
            (
                rest.split(',')
                    .next()
                    .unwrap_or_default()
                    .split(" as ")
                    .next()
                    .unwrap_or_default()
                    .trim(),
                None,
            )
        };
        let dots = module.bytes().take_while(|byte| *byte == b'.').count();
        let dotted = &module[dots..];
        let segments = dotted
            .split('.')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if segments
            .iter()
            .any(|segment| validate_symbol(segment).is_err())
            || name.is_some_and(|name| name != "*" && validate_symbol(name).is_err())
        {
            return (Vec::new(), false);
        }
        let wanted = symbol.or(name.filter(|name| *name != "*"));
        let module_path = segments.iter().collect::<PathBuf>();
        let mut candidates = Vec::new();
        let push = |candidates: &mut Vec<PathBuf>, base: &Path| {
            if let Some(name) = name.filter(|name| *name != "*") {
                candidates.push(base.join(&module_path).join(format!("{name}.py")));
            }
            if !segments.is_empty() {
                candidates.push(base.join(&module_path).with_extension("py"));
            }
            candidates.push(base.join(&module_path).join("__init__.py"));
        };
        if dots > 0 {
            let mut base = from.parent().unwrap_or(Path::new("")).to_path_buf();
            for _ in 1..dots {
                base = base.parent().map(Path::to_path_buf).unwrap_or_default();
            }
            push(&mut candidates, &base);
            return (
                self.first_existing(candidates, wanted).unwrap_or_default(),
                false,
            );
        }
        if segments.is_empty() {
            return (Vec::new(), false);
        }
        let mut truncated = false;
        let mut suffixes = Vec::new();
        if let Some(name) = name.filter(|name| *name != "*") {
            suffixes.push(module_path.join(format!("{name}.py")));
        }
        suffixes.push(module_path.with_extension("py"));
        suffixes.push(module_path.join("__init__.py"));
        for suffix in suffixes {
            let (matches, walk_truncated) = self.suffix_matches(from, &suffix);
            truncated |= walk_truncated;
            if let Some(found) = matches.first() {
                return (vec![self.locate_in(found, wanted)], truncated);
            }
        }
        (Vec::new(), truncated)
    }

    fn resolve_go(
        &mut self,
        from: &Path,
        spec: &str,
        symbol: Option<&str>,
    ) -> (Vec<CodeLocation>, bool) {
        let spec = strip_quotes(spec.trim().trim_start_matches("import").trim());
        let spec = spec.rsplit(' ').next().map_or(spec, strip_quotes);
        let mut directory = None;
        for ancestor in from.ancestors().skip(1) {
            let manifest = ancestor.join("go.mod");
            if let Some(text) = self.read_manifest(&manifest)
                && let Some(module) = text
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("module "))
                    .map(|module| strip_quotes(module.trim()))
            {
                if spec == module {
                    directory = Some(ancestor.to_path_buf());
                } else if let Some(rest) = spec.strip_prefix(&format!("{module}/")) {
                    directory = join_relative(ancestor, rest);
                }
                break;
            }
        }
        let mut truncated = false;
        let directory = if let Some(directory) = directory {
            directory
        } else {
            // Without a module match, accept a unique directory suffix.
            let suffix = Path::new(spec);
            if validate_relative_path(spec, false).is_err() {
                return (Vec::new(), false);
            }
            let (files, walk_truncated) = self.files();
            truncated |= walk_truncated;
            let mut directories = files
                .iter()
                .filter_map(|file| file.relative.parent())
                .filter(|parent| parent.ends_with(suffix))
                .map(Path::to_path_buf)
                .collect::<Vec<_>>();
            directories.sort();
            directories.dedup();
            match directories.as_slice() {
                [only] => only.clone(),
                _ => return (Vec::new(), truncated),
            }
        };
        self.package_files(&directory, symbol, Lang::Go, truncated)
    }

    fn package_files(
        &mut self,
        directory: &Path,
        symbol: Option<&str>,
        lang: Lang,
        mut truncated: bool,
    ) -> (Vec<CodeLocation>, bool) {
        let (files, walk_truncated) = self.files();
        truncated |= walk_truncated;
        let mut members = files
            .iter()
            .filter(|file| file.relative.parent() == Some(directory))
            .filter(|file| Lang::of(&file.relative) == Some(lang))
            .map(|file| file.relative.clone())
            .collect::<Vec<_>>();
        members.sort_by_key(|path| (path.to_string_lossy().ends_with("_test.go"), path.clone()));
        truncated |= members.len() > MAX_PACKAGE_FILES;
        members.truncate(MAX_PACKAGE_FILES);
        if let Some(symbol) = symbol {
            for member in &members {
                let location = self.locate_in(member, Some(symbol));
                if location.kind != "file" {
                    return (vec![location], truncated);
                }
            }
        }
        let results = members
            .iter()
            .map(|member| self.locate_in(member, None))
            .collect();
        (results, truncated)
    }

    fn resolve_include(
        &mut self,
        from: &Path,
        spec: &str,
        symbol: Option<&str>,
    ) -> (Vec<CodeLocation>, bool) {
        let spec = spec.trim().trim_start_matches("#include").trim();
        let local = spec.starts_with('"');
        let target = spec
            .trim_matches(|character| matches!(character, '"' | '<' | '>'))
            .trim();
        if target.is_empty() {
            return (Vec::new(), false);
        }
        let directory = from.parent().unwrap_or(Path::new(""));
        if local
            && let Some(candidate) = join_relative(directory, target)
            && self.exists(&candidate)
        {
            return (vec![self.locate_in(&candidate, symbol)], false);
        }
        self.resolve_suffix(from, target, symbol)
    }

    fn resolve_ruby(
        &mut self,
        from: &Path,
        spec: &str,
        symbol: Option<&str>,
    ) -> (Vec<CodeLocation>, bool) {
        let spec = spec.trim();
        let (relative, target) = if let Some(rest) = spec.strip_prefix("require_relative") {
            (true, strip_quotes(rest.trim()))
        } else {
            (
                false,
                strip_quotes(spec.trim_start_matches("require").trim()),
            )
        };
        let target = if Path::new(target).extension().is_some() {
            target.to_owned()
        } else {
            format!("{target}.rb")
        };
        if relative {
            let directory = from.parent().unwrap_or(Path::new(""));
            let candidates = join_relative(directory, &target).into_iter();
            return (
                self.first_existing(candidates, symbol).unwrap_or_default(),
                false,
            );
        }
        self.resolve_suffix(from, &target, symbol)
    }

    fn resolve_php(
        &mut self,
        from: &Path,
        spec: &str,
        symbol: Option<&str>,
    ) -> (Vec<CodeLocation>, bool) {
        let spec = spec
            .trim()
            .trim_start_matches("use")
            .trim()
            .trim_end_matches(';')
            .trim()
            .trim_start_matches('\\');
        let path = spec.split(" as ").next().unwrap_or(spec).trim();
        let segments = path
            .split('\\')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if segments.is_empty()
            || segments
                .iter()
                .any(|segment| validate_symbol(segment).is_err())
        {
            return (Vec::new(), false);
        }
        let wanted = symbol.or(segments.last().copied());
        let mut truncated = false;
        // PSR-4 maps a namespace prefix to a directory; drop leading
        // segments until a file matches.
        for skip in 0..segments.len() {
            let mut suffix = segments[skip..segments.len() - 1]
                .iter()
                .collect::<PathBuf>();
            suffix.push(format!("{}.php", segments[segments.len() - 1]));
            let (matches, walk_truncated) = self.suffix_matches(from, &suffix);
            truncated |= walk_truncated;
            if let Some(found) = matches.first() {
                return (vec![self.locate_in(found, wanted)], truncated);
            }
        }
        (Vec::new(), truncated)
    }

    fn resolve_generic(
        &mut self,
        from: &Path,
        spec: &str,
        symbol: Option<&str>,
    ) -> (Vec<CodeLocation>, bool) {
        let target = strip_quotes(spec);
        let directory = from.parent().unwrap_or(Path::new(""));
        if let Some(candidate) = join_relative(directory, target)
            && self.exists(&candidate)
        {
            return (vec![self.locate_in(&candidate, symbol)], false);
        }
        self.resolve_suffix(from, target, symbol)
    }

    fn resolve_suffix(
        &mut self,
        from: &Path,
        target: &str,
        symbol: Option<&str>,
    ) -> (Vec<CodeLocation>, bool) {
        let target = target.trim_start_matches("./");
        let Ok(suffix) = validate_relative_path(target, false) else {
            return (Vec::new(), false);
        };
        let (matches, truncated) = self.suffix_matches(from, &suffix);
        let results = matches
            .iter()
            .take(MAX_PACKAGE_FILES)
            .map(|found| self.locate_in(found, symbol))
            .collect();
        (results, truncated || matches.len() > MAX_PACKAGE_FILES)
    }

    fn read_manifest(&self, relative: &Path) -> Option<String> {
        if ensure_visible_path(relative).is_err() {
            return None;
        }
        let (file, _) = open_at(&self.root, relative, false)?;
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES).read_to_end(&mut bytes).ok()?;
        String::from_utf8(bytes).ok()
    }
}

fn finish(
    found: Vec<(u8, bool, usize, CodeLocation)>,
    mut truncated: bool,
) -> (Vec<CodeLocation>, bool) {
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for (_, _, _, location) in found {
        if !seen.insert((location.path.clone(), location.line, location.column)) {
            continue;
        }
        if results.len() == MAX_NAV_RESULTS {
            truncated = true;
            break;
        }
        results.push(location);
    }
    (results, truncated)
}

fn strip_quotes(value: &str) -> &str {
    let value = value.trim().trim_end_matches(';').trim();
    value
        .strip_prefix(['"', '\'', '`'])
        .and_then(|inner| inner.strip_suffix(['"', '\'', '`']))
        .unwrap_or(value)
}

/// Joins `spec` (with `.`/`..` segments) onto `base`. Returns `None` for an
/// absolute spec or one that climbs above the project root.
fn join_relative(base: &Path, spec: &str) -> Option<PathBuf> {
    if spec.is_empty() || spec.starts_with('/') || spec.contains('\\') || spec.contains('\0') {
        return None;
    }
    let mut parts = base
        .components()
        .map(|component| match component {
            Component::Normal(name) => Some(name.to_owned()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    for segment in spec.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            name => parts.push(name.into()),
        }
    }
    let path = parts.iter().collect::<PathBuf>();
    (!path.as_os_str().is_empty()).then_some(path)
}

fn web_candidates(base: &Path, lang: Lang) -> Vec<PathBuf> {
    const SCRIPT_EXTENSIONS: &[&str] = &[
        "ts", "tsx", "d.ts", "js", "jsx", "mjs", "cjs", "mts", "cts", "json", "vue", "svelte",
    ];
    const STYLE_EXTENSIONS: &[&str] = &["css", "scss", "sass", "less"];
    let extensions = if lang == Lang::Css {
        STYLE_EXTENSIONS
    } else {
        SCRIPT_EXTENSIONS
    };
    let mut candidates = vec![base.to_path_buf()];
    let name = base
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_owned();
    // TypeScript ESM imports name the emitted `.js` file.
    if let Some(stem) = [".js", ".jsx", ".mjs", ".cjs"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
    {
        for extension in ["ts", "tsx", "mts", "cts"] {
            candidates.push(base.with_file_name(format!("{stem}.{extension}")));
        }
    }
    for extension in extensions {
        candidates.push(base.with_file_name(format!("{name}.{extension}")));
    }
    if lang == Lang::Css {
        candidates.push(base.with_file_name(format!("_{name}.scss")));
    }
    for extension in extensions {
        candidates.push(base.join(format!("index.{extension}")));
    }
    candidates
}

fn rust_module_directory(from: &Path) -> PathBuf {
    let directory = from.parent().unwrap_or(Path::new("")).to_path_buf();
    match from.file_name().and_then(OsStr::to_str) {
        Some("lib.rs" | "main.rs" | "mod.rs") | None => directory,
        Some(name) => directory.join(name.trim_end_matches(".rs")),
    }
}

fn directory_distance(left: &Path, right: &Path) -> usize {
    let left = left.components().collect::<Vec<_>>();
    let right = right.components().collect::<Vec<_>>();
    let common = left
        .iter()
        .zip(&right)
        .take_while(|(left, right)| left == right)
        .count();
    (left.len() - common) + (right.len() - common)
}

fn preview(line: &str) -> String {
    line.trim()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_PREVIEW_CHARS)
        .collect()
}

struct WalkState {
    files: Vec<WalkedFile>,
    directories: usize,
    truncated: bool,
    deadline: Instant,
}

fn visible_name(name: &str) -> bool {
    !name.starts_with('.') && safe_component(name) && !sensitive_component(name)
}

/// Depth-first, descriptor-relative walk. At most one directory descriptor
/// per level stays open, symlinks are never followed, and hidden, sensitive,
/// and generated trees are never entered.
fn walk_directory(directory: &File, relative: &Path, depth: usize, state: &mut WalkState) {
    if state.truncated {
        return;
    }
    let Ok(mut reader) = Dir::read_from(directory) else {
        return;
    };
    let mut children = Vec::new();
    let mut scanned = 0_usize;
    while let Some(entry) = reader.read() {
        if Instant::now() >= state.deadline || state.files.len() >= MAX_NAV_FILES {
            state.truncated = true;
            return;
        }
        scanned += 1;
        if scanned > MAX_NAV_DIRECTORY_ENTRIES {
            state.truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        if matches!(name, "." | "..") || !visible_name(name) {
            continue;
        }
        let Ok(descriptor) = reader.fd() else {
            continue;
        };
        let Ok(stat) = rustix::fs::statat(descriptor, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
        else {
            continue;
        };
        let file_type = FileType::from_raw_mode(stat.st_mode);
        if file_type.is_dir() {
            if !SKIPPED_DIRECTORIES.contains(&name) {
                children.push(name.to_owned());
            }
        } else if file_type.is_file() {
            state.files.push(WalkedFile {
                relative: relative.join(name),
                size: u64::try_from(stat.st_size).unwrap_or(u64::MAX),
            });
        }
    }
    children.sort();
    for name in children {
        if state.truncated {
            return;
        }
        if depth + 1 > MAX_NAV_DEPTH || state.directories >= MAX_NAV_DIRECTORIES {
            state.truncated = true;
            return;
        }
        let Some((child, _)) = open_at(directory, Path::new(&name), true) else {
            continue;
        };
        state.directories += 1;
        walk_directory(&child, &relative.join(&name), depth + 1, state);
    }
}

/// Opens `relative` below `root` one component at a time, refusing symlinks
/// and any component whose identity changes between `stat` and `open`.
fn open_at(root: &File, relative: &Path, directory: bool) -> Option<(File, u64)> {
    let names = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => name.to_str().filter(|name| visible_name(name)),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    if names.is_empty() {
        return None;
    }
    let mut current: Option<File> = None;
    let mut size = 0;
    for (index, name) in names.iter().enumerate() {
        let parent = current.as_ref().unwrap_or(root);
        let last = index + 1 == names.len();
        let stat = rustix::fs::statat(parent, *name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
        let file_type = FileType::from_raw_mode(stat.st_mode);
        let want_directory = !last || directory;
        if file_type.is_symlink()
            || (want_directory && !file_type.is_dir())
            || (!want_directory && !file_type.is_file())
        {
            return None;
        }
        let mut flags =
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY;
        if want_directory {
            flags |= OFlags::DIRECTORY;
        }
        let opened = File::from(rustix::fs::openat(parent, *name, flags, Mode::empty()).ok()?);
        let metadata = opened.metadata().ok()?;
        #[allow(clippy::unnecessary_cast)]
        let same = metadata.dev() == stat.st_dev as u64 && metadata.ino() == stat.st_ino as u64;
        if !same || metadata.is_dir() != want_directory {
            return None;
        }
        size = metadata.len();
        current = Some(opened);
    }
    current.map(|file| (file, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fmt::Write as _, fs, os::unix::fs::symlink};

    fn fixture(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "atmux-code-nav-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn ask(root: &Path, request: &CodeNavRequest) -> CodeNavResponse {
        navigate_in_root(root, request, Instant::now() + NAV_TIME_BUDGET).unwrap()
    }

    fn definitions(root: &Path, symbol: &str, from: &str) -> Vec<(String, usize, String)> {
        ask(
            root,
            &CodeNavRequest::Definitions {
                symbol: symbol.to_owned(),
                path: Some(from.to_owned()),
            },
        )
        .results
        .into_iter()
        .map(|location| (location.path, location.line, location.kind))
        .collect()
    }

    fn resolve(root: &Path, from: &str, spec: &str, symbol: Option<&str>) -> Vec<(String, usize)> {
        ask(
            root,
            &CodeNavRequest::Resolve {
                path: from.to_owned(),
                spec: spec.to_owned(),
                symbol: symbol.map(ToOwned::to_owned),
            },
        )
        .results
        .into_iter()
        .map(|location| (location.path, location.line))
        .collect()
    }

    fn kinds(lang: Lang, source: &str, symbol: &str) -> Vec<(usize, &'static str)> {
        let mut context = LineContext::default();
        let mut found = Vec::new();
        for (index, line) in source.lines().enumerate() {
            let code = blank_non_code(line, lang, &mut context.carry);
            for start in occurrences(&code, symbol.as_bytes()) {
                if let Some(kind) = declaration_kind(lang, &code, start, symbol, &context) {
                    found.push((index + 1, kind));
                }
            }
            if lang == Lang::Go {
                update_go_group(&String::from_utf8_lossy(&code), &mut context);
            }
        }
        found
    }

    #[test]
    fn symbols_are_single_bounded_identifiers() {
        for valid in [
            "Foo",
            "_private",
            "$scope",
            "snake_case_1",
            &"a".repeat(128),
        ] {
            assert!(validate_symbol(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "1abc",
            "a.b",
            "a b",
            "../etc",
            "foo()",
            "a*",
            "東京",
            &"a".repeat(129),
        ] {
            assert!(validate_symbol(invalid).is_err(), "{invalid}");
        }
        assert!(validate_spec("./ok").is_ok());
        assert!(validate_spec("bad\nspec").is_err());
        assert!(validate_spec(&"x".repeat(MAX_SPEC_BYTES + 1)).is_err());
        assert!(validate_spec("   ").is_err());
    }

    #[test]
    fn java_declarations_are_found_and_calls_are_not() {
        let source = r#"package com.acme.billing;

@Service
public final class InvoiceService extends Base implements Api {
    private static final int LIMIT = 4;
    private final Repository repository;
    public InvoiceService(Repository repository) {
        this.repository = repository;
    }
    @Override
    public List<Invoice> findAll(String owner) throws IOException {
        String text = """
            InvoiceService findAll is mentioned in a text block
            """;
        return repository.findAll(owner);
    }
    // findAll in a comment
    abstract void findAll();
}
interface Api { Invoice findAll(String owner); }
record Totals(long net) {}
enum Status { OPEN }
@interface Audited {}
"#;
        assert_eq!(
            kinds(Lang::Java, source, "InvoiceService"),
            vec![(4, "class"), (7, "method")]
        );
        assert_eq!(
            kinds(Lang::Java, source, "findAll"),
            vec![(11, "method"), (18, "method"), (20, "method")]
        );
        assert_eq!(kinds(Lang::Java, source, "repository"), vec![(6, "field")]);
        assert_eq!(kinds(Lang::Java, source, "LIMIT"), vec![(5, "field")]);
        assert_eq!(kinds(Lang::Java, source, "Totals"), vec![(21, "record")]);
        assert_eq!(kinds(Lang::Java, source, "Status"), vec![(22, "enum")]);
        assert_eq!(
            kinds(Lang::Java, source, "Audited"),
            vec![(23, "interface")]
        );
    }

    #[test]
    fn rust_declarations_raw_strings_and_lifetimes() {
        let source = r##"
pub(crate) struct Parser<'a> { input: &'a str }
impl<'a> Parser<'a> {
    pub fn parse(&self) -> Result<()> {
        let text = r#"fn parse() is only text"#;
        let quote = '"';
        self.parse_inner()
    }
}
pub trait Parse { fn parse(&self); }
macro_rules! parse { () => {} }
const LIMIT: usize = 4;
pub(crate) mod parse_tests;
enum Token { Word }
type Alias = Parser<'static>;
"##;
        assert_eq!(
            kinds(Lang::Rust, source, "parse"),
            vec![(4, "function"), (10, "function"), (11, "macro")]
        );
        assert_eq!(
            kinds(Lang::Rust, source, "Parser"),
            vec![(2, "struct"), (3, "impl")]
        );
        assert_eq!(kinds(Lang::Rust, source, "LIMIT"), vec![(12, "constant")]);
        assert_eq!(
            kinds(Lang::Rust, source, "parse_tests"),
            vec![(13, "module")]
        );
        assert_eq!(kinds(Lang::Rust, source, "Token"), vec![(14, "enum")]);
        assert_eq!(kinds(Lang::Rust, source, "Alias"), vec![(15, "type")]);
    }

    #[test]
    fn typescript_declarations_members_and_templates() {
        let source = "export interface Shape<T> { area(): number; label?: string }
export type Area = number;
export class Circle implements Shape<number> {
  private radius: number;
  constructor(radius: number) { this.radius = radius; }
  area(): number {
    const note = `area ${this.radius}`;
    return Math.PI * this.radius;
  }
  static fromJson(json: string): Circle {
    return new Circle(JSON.parse(json).radius);
  }
}
export const DEFAULT_RADIUS = 2;
export function makeCircle<T>(radius = DEFAULT_RADIUS) { return new Circle(radius); }
describe(\"area\", () => {
  makeCircle();
});
enum Units { Metric }
";
        assert_eq!(
            kinds(Lang::TypeScript, source, "Circle"),
            vec![(3, "class")]
        );
        assert_eq!(kinds(Lang::TypeScript, source, "area"), vec![(6, "method")]);
        assert_eq!(
            kinds(Lang::TypeScript, source, "radius"),
            vec![(4, "property")]
        );
        assert_eq!(
            kinds(Lang::TypeScript, source, "fromJson"),
            vec![(10, "method")]
        );
        assert_eq!(
            kinds(Lang::TypeScript, source, "DEFAULT_RADIUS"),
            vec![(14, "variable")]
        );
        assert_eq!(
            kinds(Lang::TypeScript, source, "makeCircle"),
            vec![(15, "function")]
        );
        assert_eq!(
            kinds(Lang::TypeScript, source, "Shape"),
            vec![(1, "interface")]
        );
        assert_eq!(kinds(Lang::TypeScript, source, "Area"), vec![(2, "type")]);
        assert_eq!(kinds(Lang::TypeScript, source, "Units"), vec![(19, "enum")]);
        assert!(kinds(Lang::TypeScript, source, "describe").is_empty());
    }

    #[test]
    fn python_and_go_declarations() {
        let python = "MAX_ITEMS = 10
class Cart:
    \"\"\"Cart docs mention add_item
    across lines.\"\"\"
    def __init__(self):
        self.items = []
    async def add_item(self, item):
        self.items.append(item)

def add_item(cart, item):
    cart.add_item(item)
";
        assert_eq!(kinds(Lang::Python, python, "Cart"), vec![(2, "class")]);
        assert_eq!(
            kinds(Lang::Python, python, "add_item"),
            vec![(7, "function"), (10, "function")]
        );
        assert_eq!(
            kinds(Lang::Python, python, "MAX_ITEMS"),
            vec![(1, "variable")]
        );
        assert_eq!(kinds(Lang::Python, python, "items"), vec![(6, "attribute")]);

        let go = "package store

const (
\tDefaultLimit = 10
\tMaxLimit     = 100
)

type Store struct{ limit int }

type (
\tID string
)

func NewStore() *Store { return &Store{limit: DefaultLimit} }

func (s *Store) Limit() int { return s.limit }
";
        assert_eq!(kinds(Lang::Go, go, "DefaultLimit"), vec![(4, "constant")]);
        assert_eq!(kinds(Lang::Go, go, "Store"), vec![(8, "struct")]);
        assert_eq!(kinds(Lang::Go, go, "ID"), vec![(11, "type")]);
        assert_eq!(kinds(Lang::Go, go, "NewStore"), vec![(14, "function")]);
        assert_eq!(kinds(Lang::Go, go, "Limit"), vec![(16, "method")]);
    }

    #[test]
    fn project_definitions_prefer_the_same_family_and_nearer_files() {
        let root = fixture("definitions");
        write(
            &root,
            "app/src/main/java/com/acme/Invoice.java",
            "package com.acme;\npublic class Invoice {}\n",
        );
        write(
            &root,
            "app/src/main/java/com/acme/Billing.java",
            "package com.acme;\nclass Billing { Invoice make() { return new Invoice(); } }\n",
        );
        write(&root, "web/invoice.ts", "export class Invoice {}\n");
        write(&root, "other/far/away/Invoice.kt", "class Invoice\n");
        let results = definitions(&root, "Invoice", "app/src/main/java/com/acme/Billing.java");
        assert_eq!(
            results,
            vec![
                (
                    "app/src/main/java/com/acme/Invoice.java".to_owned(),
                    2,
                    "class".to_owned()
                ),
                (
                    "other/far/away/Invoice.kt".to_owned(),
                    1,
                    "class".to_owned()
                ),
            ]
        );
        let references = ask(
            &root,
            &CodeNavRequest::References {
                symbol: "Invoice".to_owned(),
                path: Some("app/src/main/java/com/acme/Billing.java".to_owned()),
            },
        );
        let first = &references.results[0];
        assert_eq!(first.path, "app/src/main/java/com/acme/Billing.java");
        assert_eq!(first.kind, "reference");
        assert!(
            references
                .results
                .iter()
                .any(|location| location.kind == "definition")
        );
        assert!(!references.results.iter().any(|location| {
            Path::new(&location.path)
                .extension()
                .is_some_and(|extension| extension == "ts")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn imports_resolve_for_each_language_family() {
        let root = fixture("resolve");
        write(
            &root,
            "src/main/java/com/acme/model/Invoice.java",
            "package com.acme.model;\n\npublic class Invoice {\n  public static Invoice empty() { return null; }\n}\n",
        );
        write(
            &root,
            "src/main/java/com/acme/App.java",
            "import com.acme.model.Invoice;\n",
        );
        write(
            &root,
            "web/src/util/format.ts",
            "\nexport function formatDate() {}\n",
        );
        write(&root, "web/src/components/index.ts", "export {};\n");
        write(
            &root,
            "web/src/app.tsx",
            "import { formatDate } from './util/format';\n",
        );
        write(&root, "crate/Cargo.toml", "[package]\nname = \"demo\"\n");
        write(
            &root,
            "crate/src/lib.rs",
            "pub mod net;\npub fn root_item() {}\n",
        );
        write(&root, "crate/src/net.rs", "pub mod http;\n");
        write(&root, "crate/src/net/http.rs", "\npub struct Client;\n");
        write(&root, "py/pkg/__init__.py", "");
        write(&root, "py/pkg/models.py", "\n\nclass User:\n    pass\n");
        write(&root, "py/app.py", "from pkg.models import User\n");
        write(&root, "go/go.mod", "module example.com/demo\n\ngo 1.22\n");
        write(
            &root,
            "go/store/store.go",
            "package store\n\nfunc Open() {}\n",
        );
        write(&root, "go/store/store_test.go", "package store\n");
        write(&root, "go/main.go", "package main\n");
        write(&root, "native/include/util.h", "int util(void);\n");
        write(
            &root,
            "native/src/main.c",
            "#include \"../include/util.h\"\n",
        );

        assert_eq!(
            resolve(
                &root,
                "src/main/java/com/acme/App.java",
                "import com.acme.model.Invoice;",
                None
            ),
            vec![("src/main/java/com/acme/model/Invoice.java".to_owned(), 3)]
        );
        assert_eq!(
            resolve(
                &root,
                "src/main/java/com/acme/App.java",
                "import static com.acme.model.Invoice.empty;",
                None
            ),
            vec![("src/main/java/com/acme/model/Invoice.java".to_owned(), 4)]
        );
        assert_eq!(
            resolve(
                &root,
                "web/src/app.tsx",
                "./util/format",
                Some("formatDate")
            ),
            vec![("web/src/util/format.ts".to_owned(), 2)]
        );
        assert_eq!(
            resolve(&root, "web/src/app.tsx", "./components", None),
            vec![("web/src/components/index.ts".to_owned(), 1)]
        );
        assert_eq!(
            resolve(&root, "crate/src/lib.rs", "mod net", None),
            vec![("crate/src/net.rs".to_owned(), 1)]
        );
        assert_eq!(
            resolve(&root, "crate/src/net.rs", "mod http", None),
            vec![("crate/src/net/http.rs".to_owned(), 1)]
        );
        assert_eq!(
            resolve(
                &root,
                "crate/src/net/http.rs",
                "use crate::net::http::Client;",
                None
            ),
            vec![("crate/src/net/http.rs".to_owned(), 2)]
        );
        assert_eq!(
            resolve(
                &root,
                "crate/src/net/http.rs",
                "use crate::root_item;",
                None
            ),
            vec![("crate/src/lib.rs".to_owned(), 2)]
        );
        assert_eq!(
            resolve(&root, "py/app.py", "from pkg.models import User", None),
            vec![("py/pkg/models.py".to_owned(), 3)]
        );
        assert_eq!(
            resolve(
                &root,
                "go/main.go",
                "\"example.com/demo/store\"",
                Some("Open")
            ),
            vec![("go/store/store.go".to_owned(), 3)]
        );
        assert_eq!(
            resolve(&root, "go/main.go", "\"example.com/demo/store\"", None)
                .into_iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>(),
            vec![
                "go/store/store.go".to_owned(),
                "go/store/store_test.go".to_owned()
            ]
        );
        assert_eq!(
            resolve(
                &root,
                "native/src/main.c",
                "#include \"../include/util.h\"",
                None
            ),
            vec![("native/include/util.h".to_owned(), 1)]
        );
        // Escaping the root and unknown packages resolve to nothing.
        assert!(resolve(&root, "web/src/app.tsx", "../../../../etc/passwd", None).is_empty());
        assert!(resolve(&root, "web/src/app.tsx", "react", None).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn symlinks_hidden_sensitive_and_generated_paths_are_never_returned() {
        let root = fixture("safety");
        let outside = fixture("safety-outside");
        write(&outside, "Secret.java", "class Secret {}\n");
        write(&root, "src/Main.java", "class Main { Secret secret; }\n");
        write(&root, ".hidden/Secret.java", "class Secret {}\n");
        write(&root, "node_modules/pkg/Secret.java", "class Secret {}\n");
        write(&root, "target/Secret.java", "class Secret {}\n");
        write(&root, ".env", "Secret=1\n");
        write(&root, "keys/Secret.pem", "class Secret {}\n");
        symlink(outside.join("Secret.java"), root.join("src/Linked.java")).unwrap();
        symlink(&outside, root.join("linked-dir")).unwrap();
        let results = definitions(&root, "Secret", "src/Main.java");
        assert!(results.is_empty(), "{results:?}");
        let references = ask(
            &root,
            &CodeNavRequest::References {
                symbol: "Secret".to_owned(),
                path: None,
            },
        );
        assert_eq!(
            references
                .results
                .iter()
                .map(|location| location.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/Main.java"]
        );
        assert!(resolve(&root, "src/Main.java", "import Linked;", None).is_empty());
        assert!(
            navigate_in_root(
                &root,
                &CodeNavRequest::Definitions {
                    symbol: "Secret".to_owned(),
                    path: Some("../outside/Secret.java".to_owned()),
                },
                Instant::now() + NAV_TIME_BUDGET,
            )
            .is_err()
        );
        assert!(
            navigate_in_root(
                &root,
                &CodeNavRequest::Definitions {
                    symbol: "Secret".to_owned(),
                    path: Some(".git/config".to_owned()),
                },
                Instant::now() + NAV_TIME_BUDGET,
            )
            .is_err()
        );
        assert!(
            navigate_in_root(
                &root,
                &CodeNavRequest::Definitions {
                    symbol: "Se.cret".to_owned(),
                    path: None,
                },
                Instant::now() + NAV_TIME_BUDGET,
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn budgets_stop_the_scan_and_report_truncation() {
        let root = fixture("budget");
        for index in 0..30 {
            write(
                &root,
                &format!("src/file{index}.rs"),
                "fn target() {}\nfn target_two() { target(); }\n",
            );
        }
        let expired = navigate_in_root(
            &root,
            &CodeNavRequest::Definitions {
                symbol: "target".to_owned(),
                path: None,
            },
            Instant::now(),
        )
        .unwrap();
        assert!(expired.truncated);

        let mut project = Project::new(
            open_absolute_directory(&root).unwrap(),
            Instant::now() + NAV_TIME_BUDGET,
        );
        project.bytes_read = MAX_NAV_BYTES;
        let (results, truncated) = project.references("target", None);
        assert!(results.is_empty());
        assert!(truncated);

        let many = (0..MAX_NAV_RESULTS + 20).fold(String::new(), |mut output, index| {
            let _ = writeln!(output, "fn f{index}() {{ target(); }}");
            output
        });
        write(&root, "src/many.rs", &many);
        let references = ask(
            &root,
            &CodeNavRequest::References {
                symbol: "target".to_owned(),
                path: Some("src/many.rs".to_owned()),
            },
        );
        assert_eq!(references.results.len(), MAX_NAV_RESULTS);
        assert!(references.truncated);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn previews_are_bounded_and_control_free() {
        let line = format!("\tfn x() {{ {} }}\u{7}", "y".repeat(400));
        let value = preview(&line);
        assert_eq!(value.chars().count(), MAX_PREVIEW_CHARS);
        assert!(!value.chars().any(char::is_control));
        assert_eq!(
            join_relative(Path::new("a/b"), "../c"),
            Some(PathBuf::from("a/c"))
        );
        assert_eq!(join_relative(Path::new("a"), "../../c"), None);
        assert_eq!(join_relative(Path::new("a"), "/etc/passwd"), None);
        assert_eq!(directory_distance(Path::new("a/b/c"), Path::new("a/d")), 3);
    }
}
