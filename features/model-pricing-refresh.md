# Model and pricing refresh — September 2026

Request: “will you update model costs and add new models”. Preserve Fast as a
separate control, not a model variant.

## Implementation

- Refresh the authoritative Pulse catalog to 2026-09-07. Add missing rates for
  GPT-6 Astra and GPT-5.6 Sol/Terra/Luna; resolve the exact `gpt-5.6` alias to Sol.
  The existing Codex picker already offers those four models, independently of
  effort and Fast, so no duplicate picker entries or changed defaults are needed.
- Add Claude Fable/Mythos 5.1, Opus 5, Sonnet 5 and specific Opus 4.5–4.8 prices.
  Correct older Opus 4/4.1 prices, add Haiku 3.5, and retain historical families.
  Opus 5 and Sonnet 5 context meters now recognize their 1M windows. Claude's
  native picker aliases remain version-floating.
- Add Gemini 3.6/3.7/3.8 Flash and 3.5/3.1 Flash-Lite. Correct legacy Gemini 3
  Flash prices. Update DeepSeek V4 Pro/Flash and add Flash Vision experimental.
- Match published settings-specific prices: OpenAI Fast/priority, batch/flex and
  long context; Claude batch and supported fast-speed models; newer Gemini Flash
  batch/flex/priority; DeepSeek off-peak when explicitly identified.
- Current built-in defaults supersede stale seeded copies at read time, including
  the paginated REST/MCP catalog. Idempotent reseeding preserves account overrides
  and custom default keys. No schema, authorization, or account-scope changes.
- Spark has no verified public API list price. It stays selectable but is marked
  fallback-priced, never assigned ordinary Codex's authoritative price. Retire
  fabricated generic DeepSeek and unidentified Antigravity defaults similarly;
  explicit account overrides still work. Legacy rows are ignored, not deleted.

## Sources and estimate boundaries

Primary sources checked on 2026-09-07 using the OpenAI Docs skill for OpenAI and
the other providers' official documentation:

- [OpenAI pricing](https://developers.openai.com/api/docs/pricing),
  [Sol model/alias](https://developers.openai.com/api/docs/models/gpt-5.6-sol),
  [GPT-5.5](https://developers.openai.com/api/docs/models/gpt-5.5),
  [GPT-5.5 Pro](https://developers.openai.com/api/docs/models/gpt-5.5-pro),
  [GPT-5.4](https://developers.openai.com/api/docs/models/gpt-5.4), and
  [Codex models](https://learn.chatgpt.com/docs/models).
- [Claude pricing](https://platform.claude.com/docs/en/about-claude/pricing),
  [model identifiers/context](https://platform.claude.com/docs/en/models/overview),
  and [delivered Fast speed](https://platform.claude.com/docs/en/build-with-claude/fast-mode).
- [Gemini pricing](https://ai.google.dev/gemini-api/docs/pricing).
- [DeepSeek pricing](https://api-docs.deepseek.com/quick_start/pricing/).

These remain current API list-price equivalents, not subscription invoices or a
historically effective-dated billing engine. Sol's promotional price is promised
at least through 2026-11-21. Gemini 3.6/3.7/3.8 Flash prices apply through
2026-12-31; refresh before January. Sonnet 5's lower price is now permanent.
Time-based cache storage, tools, audio/image output, and regional uplifts are not
represented by the existing five token classes. GPT-5.5 Pro's lack of a cache
discount means cached input uses the input rate, not zero.

Pricing modifiers require recorded settings: `service_tier`,
`additional.speed=fast`, `additional.context_tier=long` (>272k OpenAI request
input), or `additional.billing_period=off_peak`. Missing context/speed metadata
uses standard rates; missing DeepSeek billing-period metadata uses conservative
peak rates. Never infer request length or billing hour from daily totals.

Native transcript regrouping is deliberately deferred: persisted grains are
upserted by settings hash, so reclassifying existing logs without authoritative
partition replacement would double-count old rows. This change does not mutate
token counts or settings hashes. Automatic Claude speed/context classification
and the Codex explicit-null tier-reset parser fix need that migration first.

## Verification gates

- [x] Implementation present in the shared worktree.
- [x] Final focused unit/API/model-control tests and static checks recorded.
- [ ] Live integration on every affected platform.
- [ ] Fable/Claude Max and independent security review of the frozen snapshot.

No service restart or fleet deployment is included in this refresh. This record
stays active until the remaining project gates are completed.

Local evidence:

- `cargo test --all-features -- --test-threads=1` passed, including catalog,
  report, API pagination/reseed, account-override and separate model/Fast smoke
  tests. Opt-in live-platform tests remain subject to their existing gates.
- `node --test web/app.test.mjs web/session-actions.test.mjs web/mobile-viewport.test.mjs web/dashboard-interaction.test.mjs`:
  150 passed.
- `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --all -- --check`, and `git diff --check` passed.
- `cargo build --release --all-features` passed on Tron (Linux x86_64).
- An initial parallel Pulse run hit the existing credential-lock race test
  `post_flock_replacement_cannot_split_owners_or_reach_the_refresh_grant`; its
  isolated rerun and the full serial suite passed. Credential code is unchanged.
