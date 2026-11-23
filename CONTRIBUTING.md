# Contributing to Wakefold

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) for new commits.

```text
type(scope): short imperative summary
```

The scope is optional. Use it when one area owns the change, such as `core`,
`openai`, `macros`, `daemon`, or `examples`. Start the summary with a
lowercase verb. Use a body when the reason or migration steps need more detail.

### Types

- `feat` adds a capability. `feat(core): enforce per-run token budgets`
- `fix` corrects behavior. `fix(openai): preserve streamed tool call IDs`
- `docs` changes documentation. `docs: define commit message conventions`
- `test` adds or changes tests. `test(core): cover exhausted token budgets`
- `refactor` restructures code while preserving behavior. `refactor(core): extract usage aggregation`
- `perf` improves performance. `perf(core): reduce transcript allocations`
- `build` changes manifests, dependencies, or toolchains. `build(deps): update schemars`
- `ci` changes automation that checks or builds the project. `ci: run workspace Clippy checks`
- `style` changes formatting only. `style(macros): format imports`
- `chore` handles repository maintenance. `chore: update ignore rules`
- `revert` reverses an earlier commit. `revert: restore previous tool dispatch behavior`

Choose the type by the purpose of the change. Tests that accompany a fix belong
in the `fix` commit. Use a scope for related categories, such as
`fix(security)` for a security fix and `chore(release)` for release housekeeping.
Include the original commit hash in the body of a revert.

For a breaking change, add `!` before the colon and include a
`BREAKING CHANGE:` footer that explains how callers should migrate.

```text
feat(core)!: require run options

BREAKING CHANGE: pass RunOptions to run in place of the turn limit.
```
