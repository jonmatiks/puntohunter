# AGENTS.md

Guidance for AI coding agents working in this repository. Human contributors should read it too.

## Behavioral Guidelines

Guidelines to reduce common LLM coding mistakes. They bias toward caution over speed — for trivial tasks,
use judgment.

- **Think before coding.** State assumptions; if uncertain or something is unclear, ask. Present competing
  interpretations instead of picking one silently; if a simpler approach exists, say so.
- **Simplicity first.** Minimum code that solves the problem — no unrequested features, no abstractions for
  single-use code, no speculative "flexibility". If a senior engineer would call it overcomplicated,
  simplify.
- **Surgical changes.** Every changed line traces to the request. Don't "improve" or refactor adjacent code;
  match existing style. Remove orphans *your* change created; mention — don't delete — pre-existing dead
  code.
- **Goal-driven execution.** Turn tasks into verifiable goals ("fix the bug" → a test that reproduces it,
  then make it pass; "refactor X" → tests pass before and after); for multi-step work, state a brief
  step→verify plan.
- **Never commit without an explicit request.** Do not run `git commit`, `git push`, or anything else that
  rewrites history or publishes unless the user explicitly asks for it in the current request. Make and
  stage edits freely, then leave finished work in the working tree and say what is ready. Approval for one
  commit is not approval for the next.

## Documentation

- `docs/guide/` is the guide, published at <https://puntohunter.com/docs/>: the operator pages, `api.md` the API
  reference among them, and the development, contributing, and architecture pages. **The guide follows the
  code:** a change to a setting, an endpoint, a Stratum option, or a component updates its page, operator or
  architecture, in the same piece of work. It describes running PuntoHunter, not the project's own hosts: no host
  addresses, business plans, or internal decisions, and nothing that is planned but not built.
- **The repository stands on its own.** No comment, docstring, config comment, script, test, or page cites a
  document outside it, a milestone, or a task. Where a reason matters, it goes into the comment itself or
  onto a page of the guide. Tests and default configs use `example.com` and the documentation address
  ranges, never a real host. `python3 tests/references.py` checks both, and CI runs it.

## Formatting

- **Every `*.md` file must lint clean.** Markdown is linted by markdownlint in the editor; leave no
  warnings behind, including in files you only partly touched.
- Write table delimiter rows with spaces around every pipe and dash run (`| --- | :---: | --- |`, not
  `|---|:---:|---|`) — the compact, space-less form trips `MD060/table-column-style`.
- Run `npx markdownlint-cli2@0.23.2 "**/*.md"` before handing work over; `--fix` handles the
  mechanical rules. Pin the version: markdownlint 0.38 and earlier do not implement `MD060`, so an
  older linter reports clean on delimiter rows that the editor still flags.
- Deliberate departures from markdownlint defaults live in `.markdownlint.json`, each with the reason
  it was relaxed. Fix the Markdown rather than adding an entry unless the rule is wrong for this project.

### Conventions

- **Changelog style.** [CHANGELOG.md](CHANGELOG.md) is strictly reverse chronological: newest first, both
  across version groups *and within* a version group — a new entry goes at the top of its version's list,
  never appended at the bottom. Prefer concise changelog entries.

## Web App

- **`web/` is the portal.** The binary embeds `web/src/` and serves it on the `[portal]` listener
  (`src/portal/site.rs`), and any static host serves `web/build.py`'s build of it; the binary renders no
  page. A change a visitor can see (a page, its wording, layout, or design) is a change to `web/`. If the app
  needs a value the API does not send, extend the API rather than leave the app behind.
- **`build.py` and `site.rs` assemble pages by the same rules.** A change to one changes the other in the same
  piece of work: `tests/e2e/test_portal_site.py` checks every file the binary serves against `build.py`'s
  output, byte for byte.
- The app keeps the rules in `web/README.md` ("Rules the App Keeps"), and its README's page list and layout
  follow its files.

## Rust Style

- **Imports.** Merge nested paths into one `use` per crate (`use std::{io, net::SocketAddr};`). Split them
  into three blocks separated by a blank line: `std`/`core`/`alloc`, then external crates, then
  `crate`/`super`/`self`. Don't hand-sort within a block — `cargo fmt` does it (edition 2024 version-sorts).
  The project builds on stable Rust, where rustfmt cannot merge or group imports, so keep both by hand.
- **No glob imports** (`use module::*;`), except a designated `prelude` module and `use super::*;` inside a
  `#[cfg(test)]` module. Clippy enforces this through `wildcard_imports` and `enum_glob_use` in `Cargo.toml`,
  but skips test modules entirely, so the `super::*`-only limit there is kept by hand.
