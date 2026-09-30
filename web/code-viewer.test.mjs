import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import app from "./app.js";

const {
  tokenizeSource,
  highlightCode,
  codeLanguage,
  codeLanguageNavigable,
  codeSymbolValid,
  sourceImports,
  localDefinitionLines,
  chooseLocalDefinition,
  symbolQualifier,
  paneCodePath,
  codeNavResults,
  pushCodeHistory,
  stepCodeHistory,
} = app;

/// Non-plain tokens per line as `kind:text`, for compact assertions.
function kinds(source, language) {
  return tokenizeSource(source, language).map((line) => line
    .filter((token) => token.kind !== "plain")
    .map((token) => `${token.kind}:${token.text}`));
}

function roundTrips(source, language) {
  const joined = tokenizeSource(source, language)
    .map((line) => line.map((token) => token.text).join(""))
    .join("\n");
  assert.equal(joined, source, `${language} must render the exact source text`);
}

test("Java annotations, text blocks, and block comments keep their meaning across lines", () => {
  const source = [
    "@Service(\"billing\")",
    "public final class Invoice {",
    "  String text = \"\"\"",
    "      class NotCode { \"quoted\" }",
    "      \"\"\";",
    "  /* int hidden = 1;",
    "     still comment */ int count = 0x1F;",
    "  void total() { return sum(1.5e3); }",
    "}",
  ].join("\n");
  roundTrips(source, "java");
  const lines = kinds(source, "java");
  assert.deepEqual(lines[0], ["annotation:@Service", "string:\"billing\""]);
  assert.deepEqual(lines[1], ["keyword:public", "keyword:final", "keyword:class", "type:Invoice"]);
  assert.deepEqual(lines[2], ["type:String", "identifier:text", "string:\"\"\""]);
  assert.deepEqual(lines[3], ["string:      class NotCode { \"quoted\" }"]);
  assert.deepEqual(lines[4], ["string:      \"\"\""]);
  assert.deepEqual(lines[5], ["comment:/* int hidden = 1;"]);
  assert.deepEqual(lines[6], ["comment:     still comment */", "type:int", "identifier:count", "number:0x1F"]);
  assert.deepEqual(lines[7], ["keyword:void", "function:total", "keyword:return", "function:sum", "number:1.5e3"]);
});

test("Rust raw strings, lifetimes, char literals, attributes, and macros", () => {
  const source = [
    "#[derive(Debug, Clone)]",
    "pub struct Parser<'a> { input: &'a str }",
    "fn parse() { let raw = r#\"fn fake() \"quoted\"\"#; let quote = '\"'; println!(\"{}\", quote); }",
  ].join("\n");
  roundTrips(source, "rust");
  const lines = kinds(source, "rust");
  assert.deepEqual(lines[0], ["annotation:#[derive(Debug, Clone)]"]);
  assert.deepEqual(lines[1], [
    "keyword:pub", "keyword:struct", "type:Parser", "annotation:'a",
    "identifier:input", "annotation:'a", "type:str",
  ]);
  assert.deepEqual(lines[2], [
    "keyword:fn", "function:parse", "keyword:let", "identifier:raw",
    "string:r#\"fn fake() \"quoted\"\"#", "keyword:let", "identifier:quote", "string:'\"'",
    "function:println", "string:\"{}\"", "identifier:quote",
  ]);
});

test("TypeScript template literals span lines and generics stay types", () => {
  const source = "const note = `total ${count}\nline two`;\nexport function wrap<T>(value: T): Promise<T> { return value as T; }";
  roundTrips(source, "typescript");
  const lines = kinds(source, "typescript");
  assert.deepEqual(lines[0], ["keyword:const", "identifier:note", "string:`total ${count}"]);
  assert.deepEqual(lines[1], ["string:line two`"]);
  assert.deepEqual(lines[2], [
    "keyword:export", "keyword:function", "function:wrap", "type:T", "identifier:value",
    "type:T", "type:Promise", "type:T", "keyword:return", "identifier:value", "keyword:as", "type:T",
  ]);
});

test("Python triple quotes, decorators, f-strings, and comments", () => {
  const source = "@cached\ndef area(radius):\n    \"\"\"Docstring that\n    mentions def fake().\"\"\"\n    return f\"{radius}\"  # note";
  roundTrips(source, "python");
  const lines = kinds(source, "python");
  assert.deepEqual(lines[0], ["annotation:@cached"]);
  assert.deepEqual(lines[1], ["keyword:def", "function:area", "identifier:radius"]);
  assert.deepEqual(lines[2], ["string:\"\"\"Docstring that"]);
  assert.deepEqual(lines[3], ["string:    mentions def fake().\"\"\""]);
  assert.deepEqual(lines[4], ["keyword:return", "string:f\"{radius}\"", "comment:# note"]);
});

test("data, markup, and line formats round-trip and mark keys", () => {
  const samples = {
    json: "{\n  \"name\": \"atmux\",\n  \"count\": 3,\n  \"ok\": true\n}",
    yaml: "services:\n  web:\n    image: \"atmux:1\" # pinned\n    replicas: 2",
    toml: "[package]\nname = \"atmux\"\nversion = \"0.2.0\"\ndescription = \"\"\"\nmulti\n\"\"\"",
    html: "<div class=\"a\"><!-- note --><script>const x = \"<b>\";</script></div>",
    css: ".card { color: #fff; margin: 0 4px !important; }\n@media (max-width: 720px) {}",
    markdown: "# Title\n\n- item with `code`\n```js\nconst x = 1;\n```",
    dockerfile: "FROM rust:1.88 AS build\n# comment\nRUN cargo build --release",
    makefile: "CC ?= cc\nbuild: main.o\n\t$(CC) -o app $^",
    go: "package main\n\nimport \"fmt\"\n\nfunc main() { fmt.Println(`raw\nstring`) }",
    c: "#include <stdio.h>\n#define MAX 4\nint main(void) { return 0; }",
    shell: "#!/bin/sh\nname=\"$1\" # arg\necho \"${name}\"",
    sql: "SELECT id, name FROM users -- note\nWHERE id = 'x';",
  };
  for (const [language, source] of Object.entries(samples)) roundTrips(source, language);
  assert.ok(kinds(samples.json, "json")[1].includes("property:\"name\""));
  assert.ok(kinds(samples.yaml, "yaml")[2].includes("comment:# pinned"));
  assert.ok(kinds(samples.toml, "toml")[0].includes("heading:[package]"));
  assert.deepEqual(kinds(samples.toml, "toml")[4], ["string:multi"]);
  assert.ok(kinds(samples.html, "html")[0].includes("keyword:const"), "script content is highlighted as JavaScript");
  assert.ok(kinds(samples.css, "css")[0].includes("property:color"));
  assert.ok(kinds(samples.css, "css")[1].includes("keyword:@media"));
  assert.deepEqual(kinds(samples.markdown, "markdown")[0], ["heading:# Title"]);
  assert.deepEqual(kinds(samples.markdown, "markdown")[4], ["string:const x = 1;"]);
  assert.deepEqual(kinds(samples.dockerfile, "dockerfile")[0].slice(0, 1), ["keyword:FROM"]);
  assert.deepEqual(kinds(samples.go, "go")[5], ["string:string`"]);
  assert.ok(kinds(samples.c, "c")[0].includes("string:<stdio.h>"));
  assert.ok(kinds(samples.sql, "sql")[0].includes("keyword:SELECT"));
  assert.ok(kinds(samples.shell, "shell")[1].includes("comment:# arg"));
});

test("languages come from the owner hint, the file name, and a shebang", () => {
  assert.equal(codeLanguage("src/Main.java", "java"), "java");
  assert.equal(codeLanguage("app/Build.scala", "text"), "scala");
  assert.equal(codeLanguage("build.gradle", "text"), "groovy");
  assert.equal(codeLanguage("ui/App.tsx", "tsx"), "tsx");
  assert.equal(codeLanguage("Dockerfile.prod", "text"), "dockerfile");
  assert.equal(codeLanguage("Makefile", "makefile"), "makefile");
  assert.equal(codeLanguage("bin/tool", "text", "#!/usr/bin/env python3\nprint(1)"), "python");
  assert.equal(codeLanguage("bin/run", "text", "#!/bin/bash\necho"), "shell");
  assert.equal(codeLanguage("bin/serve", "", "#!/usr/bin/env node\n"), "javascript");
  assert.equal(codeLanguage("notes.txt", "text", "plain"), "text");
  assert.equal(codeLanguageNavigable("java"), true);
  assert.equal(codeLanguageNavigable("rust"), true);
  assert.equal(codeLanguageNavigable("json"), false);
  assert.equal(codeLanguageNavigable("text"), false);
});

test("Markdown code blocks keep generic highlighting and exact text", () => {
  const source = "let x = 1; // note\n# heading-like comment";
  assert.equal(highlightCode(source).map((segment) => segment.text).join(""), source);
  assert.ok(highlightCode(source).some((segment) => segment.kind === "comment"));
});

test("import extraction binds names and clickable targets per language", () => {
  const ts = sourceImports([
    "import React, { useState as useLocal, type Props } from 'react';",
    "import * as api from './api/client';",
    "import {",
    "  formatDate,",
    "} from \"../util/format\";",
    "const { join } = require('./paths');",
  ].join("\n"), "typescript");
  assert.deepEqual(ts.names.get("useLocal"), { spec: "react", symbol: "useState" });
  assert.deepEqual(ts.names.get("React"), { spec: "react", symbol: "React" });
  assert.deepEqual(ts.names.get("api"), { spec: "./api/client", symbol: null });
  assert.deepEqual(ts.names.get("formatDate"), { spec: "../util/format", symbol: "formatDate" });
  assert.deepEqual(ts.names.get("join"), { spec: "./paths", symbol: "join" });
  const clientRange = ts.lines.get(2)[0];
  assert.equal("import * as api from './api/client';".slice(clientRange.start, clientRange.end), "./api/client");

  const java = sourceImports("import com.acme.model.Invoice;\nimport static com.acme.Util.helper;\nimport java.util.*;", "java");
  assert.deepEqual(java.names.get("Invoice"), { spec: "import com.acme.model.Invoice", symbol: "Invoice" });
  assert.deepEqual(java.names.get("helper"), { spec: "import static com.acme.Util.helper", symbol: "helper" });
  assert.equal(java.names.has("*"), false);
  const range = java.lines.get(1)[0];
  assert.equal("import com.acme.model.Invoice;".slice(range.start, range.end), "com.acme.model.Invoice");

  const rust = sourceImports("use crate::net::{http::Client, Server as S};\npub mod parse;\nuse super::config::Config;", "rust");
  assert.deepEqual(rust.names.get("Client"), { spec: "use crate::net::http::Client", symbol: null });
  assert.deepEqual(rust.names.get("S"), { spec: "use crate::net::Server", symbol: null });
  assert.deepEqual(rust.names.get("Config"), { spec: "use super::config::Config", symbol: null });
  assert.equal(rust.lines.get(2)[0].spec, "mod parse");

  const python = sourceImports("from .models import (\n    User,\n    Group as G,\n)\nimport os.path as osp", "python");
  assert.deepEqual(python.names.get("User"), { spec: "from .models import User", symbol: "User" });
  assert.deepEqual(python.names.get("G"), { spec: "from .models import Group", symbol: "Group" });
  assert.deepEqual(python.names.get("osp"), { spec: "import os.path", symbol: null });

  const go = sourceImports("package main\n\nimport (\n\t\"fmt\"\n\tstore \"example.com/demo/store/v2\"\n)\n", "go");
  assert.deepEqual(go.names.get("fmt"), { spec: "\"fmt\"", symbol: null });
  assert.deepEqual(go.names.get("store"), { spec: "\"example.com/demo/store/v2\"", symbol: null });

  const c = sourceImports("#include \"util/strings.h\"\n#include <stdio.h>", "c");
  assert.equal(c.lines.get(1)[0].spec, "#include \"util/strings.h\"");
  assert.equal(c.lines.get(2)[0].spec, "#include <stdio.h>");
});

test("same-file definitions prefer the nearest declaration above the click", () => {
  const source = [
    "const total = 1;",
    "function compute(items) {",
    "  const total = items.length;",
    "  return total;",
    "}",
    "class Box {",
    "  open() {",
    "    return compute([]);",
    "  }",
    "}",
    "const handler = () => total;",
  ].join("\n");
  const tokens = tokenizeSource(source, "javascript");
  const totals = localDefinitionLines(tokens, "javascript", "total");
  assert.deepEqual(totals.map((definition) => definition.line), [1, 3]);
  assert.equal(chooseLocalDefinition(totals, 4).line, 3, "a local shadows the outer name");
  assert.equal(chooseLocalDefinition(totals, 11).line, 3);
  assert.deepEqual(localDefinitionLines(tokens, "javascript", "compute").map((definition) => definition.line), [2]);
  assert.deepEqual(localDefinitionLines(tokens, "javascript", "open").map((definition) => definition.line), [7]);
  assert.deepEqual(localDefinitionLines(tokens, "javascript", "Box").map((definition) => definition.line), [6]);
  assert.equal(chooseLocalDefinition([], 3), null);

  const java = tokenizeSource("class A {\n  private final Repo repo;\n  void run(Repo other) {\n    Repo local = repo;\n    helper(local);\n  }\n}", "java");
  assert.deepEqual(localDefinitionLines(java, "java", "repo").map((definition) => definition.line), [2]);
  assert.deepEqual(localDefinitionLines(java, "java", "local").map((definition) => definition.line), [4]);
  assert.deepEqual(localDefinitionLines(java, "java", "run").map((definition) => definition.line), [3]);
  assert.deepEqual(localDefinitionLines(java, "java", "helper"), []);

  const rust = tokenizeSource("fn main() {\n    let value = 3;\n    helper(value);\n}\nmacro_rules! helper { () => {} }", "rust");
  assert.deepEqual(localDefinitionLines(rust, "rust", "value").map((definition) => definition.line), [2]);
  assert.deepEqual(localDefinitionLines(rust, "rust", "helper").map((definition) => definition.line), [5]);
});

test("qualifiers, symbols, and owner routes are validated before any request", () => {
  assert.equal(symbolQualifier("  return api.fetchUser(id);", 14), "api");
  assert.equal(symbolQualifier("let c = crate_a::Client::new();", 18), "crate_a");
  assert.equal(symbolQualifier("fetchUser(id)", 1), null);
  assert.equal(codeSymbolValid("Invoice"), true);
  assert.equal(codeSymbolValid("$scope"), true);
  for (const invalid of ["", "1abc", "a.b", "../x", "a b", "x".repeat(129), null]) {
    assert.equal(codeSymbolValid(invalid), false, String(invalid));
  }
  const pane = "midnight~%12";
  assert.equal(
    paneCodePath(pane, "definitions", { symbol: "Invoice", path: "src/a b.java" }),
    `/api/v1/panes/${encodeURIComponent(pane)}/code/definitions?symbol=Invoice&path=src%2Fa+b.java`,
  );
  assert.equal(
    paneCodePath(pane, "resolve", { path: "web/app.ts", spec: "./util/format", symbol: "formatDate" }),
    `/api/v1/panes/${encodeURIComponent(pane)}/code/resolve?symbol=formatDate&path=web%2Fapp.ts&spec=.%2Futil%2Fformat`,
  );
  assert.equal(paneCodePath(pane, "definitions", { symbol: "a.b" }), null);
  assert.equal(paneCodePath(pane, "definitions", { symbol: "Foo", path: "../secret" }), null);
  assert.equal(paneCodePath(pane, "resolve", { path: "a.ts" }), null, "resolve needs a specifier");
  assert.equal(paneCodePath(pane, "resolve", { path: "a.ts", spec: "bad\nspec" }), null);
  assert.equal(paneCodePath(pane, "execute", { symbol: "Foo" }), null);
  assert.equal(paneCodePath(null, "definitions", { symbol: "Foo" }), null);
});

test("owner results are normalized to project-relative, bounded locations", () => {
  const normalized = codeNavResults({
    results: [
      { path: "src/Main.java", line: 3, column: 14, kind: "class", preview: "public class Main {\u0007" },
      { path: "/etc/passwd", line: 1, column: 1, kind: "file", preview: "" },
      { path: "../outside.rs", line: 2, column: 1, kind: "function", preview: "" },
      { path: "src/lib.rs", line: 0, column: 1, kind: "function", preview: "" },
      { path: "src/lib.rs", line: 7, column: -2, kind: "<script>", preview: "x".repeat(400) },
    ],
    truncated: true,
  });
  assert.equal(normalized.truncated, true);
  assert.deepEqual(normalized.results.map((result) => [result.path, result.line, result.column, result.kind]), [
    ["src/Main.java", 3, 14, "class"],
    ["src/lib.rs", 7, 1, "script"],
  ]);
  assert.equal(normalized.results[0].preview, "public class Main { ");
  assert.equal(normalized.results[1].preview.length, 240);
  assert.deepEqual(codeNavResults(null), { results: [], truncated: false });
});

test("viewer history moves back and forward and forgets forward on a new jump", () => {
  let history = { back: [], forward: [] };
  history = pushCodeHistory(history, { path: "a.rs", top: 10, left: 0 });
  history = pushCodeHistory(history, { path: "b.rs", top: 20, left: 0 });
  let step = stepCodeHistory(history, "back", { path: "c.rs", top: 30, left: 0 });
  assert.equal(step.target.path, "b.rs");
  assert.deepEqual(step.history.forward.map((entry) => entry.path), ["c.rs"]);
  step = stepCodeHistory(step.history, "forward", { path: "b.rs", top: 20, left: 0 });
  assert.equal(step.target.path, "c.rs");
  history = pushCodeHistory(stepCodeHistory(step.history, "back", step.target).history, { path: "d.rs", top: 0, left: 0 });
  assert.deepEqual(history.forward, []);
  assert.equal(stepCodeHistory({ back: [], forward: [] }, "back", null).target, null);
  let bounded = { back: [], forward: [] };
  for (let index = 0; index < 80; index += 1) bounded = pushCodeHistory(bounded, { path: `f${index}.rs`, top: 0, left: 0 });
  assert.equal(bounded.back.length, 50);
});

test("the viewer wires click-through without evaluating source or sending paths it did not validate", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const viewer = source.slice(source.indexOf("  function appendSource("), source.indexOf("  function updateFileSelection("));
  assert.doesNotMatch(viewer, /innerHTML|insertAdjacentHTML|eval\(|new Function/);
  assert.match(viewer, /paneCodePath\(paneId, operation, params\)/);
  assert.match(viewer, /confirmDiscardFileEdit\(files\)/, "navigation guards unsaved edits");
  assert.match(source, /\$\("file-viewer"\)\.addEventListener\("keydown", handleFileViewerKeydown\)/);
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  assert.match(css, /\.code-symbol-panel \{[^}]*position: sticky;/);
  assert.match(css, /\.syntax-annotation/);
});
