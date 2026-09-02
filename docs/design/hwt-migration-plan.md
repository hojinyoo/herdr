# hwt to herdr: implementation plan

Companion to [hwt-migration-spec.md](hwt-migration-spec.md). The spec settles what moves and why; this
plan says in what order, against which files, and what each stage owes the repo before it can land.

Nothing here is implementation. No stage starts before Gate 0.

## Gate 0: decisions before any code

| # | Decision | Position | Owner |
|---|---|---|---|
| D1 | Upstream contribution or fork-local carry | **Open.** herdrdev/herdr auto-closes unsolicited implementation PRs and nobody asked for this feature. Either an invitation exists, or this is a fork-local carry paying a rebase cost every release. Every later stage compounds the answer | user |
| D2 | `WorktreeInfo.path` optional, or a sibling array for the checkout-less states | Optional, one array. Callers already switch on the sibling booleans. Incompatible, so it carries the one protocol bump | settled |
| D3 | Token ownership when herdr derives tokens itself | Reserve declared keys and refuse a `workspace report-metadata` write to one with a named error. Fails loudly at the write instead of silently at the render | settled |
| D4 | Branch deletion default in `worktree remove` | Opt-in `delete_branch`, default false. hwt deletes by default; herdr does not, and flipping it silently destroys branches for existing users | settled |
| D5 | Chips persisted or re-derived | Re-derive at server start. `WorkspaceSnapshot` has no tokens field, so persisting means adding one and accepting staleness | settled |
| D6 | Derived naming default | Off. With no `[worktrees.naming]` block nothing derives, which keeps herdr ignorant of herdr-mirror instead of importing its guards (see Stage 6) | settled |
| D7 | `branch_only` entries in `worktree list` | Opt-in param, default false. Finding them costs a `git for-each-ref` per call and `worktree list` sits on interactive paths | settled |

D2 through D7 are recorded as decided so the plan is executable; any of them is cheap to overturn before
its stage starts, and expensive after.

## Obligations that apply to every stage

These are the non-obvious ones. They are enforced by `just check`, so missing one is a red build, not a
review comment.

- **API schema artifact.** Any change under `src/api/schema/**` must regenerate
  `docs/next/api/herdr-api.schema.json` with
  `HERDR_UPDATE_API_SCHEMA=1 just test-one generated_protocol_schema_artifact_is_current`. The file is
  `include_str!`-compiled into the binary through `src/cli/api.rs`, so a stale artifact is a lie the
  binary tells, not documentation drift.
- **Config reference.** Any new key in `src/config/*.rs` must appear in
  `docs/next/website/src/data/config-reference.json` or `scripts/config_reference_check.py` fails.
  Arrays of tables are not enumerable per key and live in that script's `SKIPPED_SUBTREES`, so
  `[[worktrees.path]]` needs a skip entry rather than per-key rows. Confirm which before writing rows.
- **Translation parity.** A new or restructured `.mdx` under `docs/next/website/src/content/docs/` needs
  ja and zh-cn counterparts with the same heading outline (`scripts/docs_translation_parity.py`).
- **Protocol version.** Source is `PROTOCOL_VERSION = 20`; stable 0.8.2 published 20 and the current
  preview published 20. Protocol 20 is therefore already out in both channels, so the **first**
  incompatible wire change on this line bumps to 21, and every later incompatible change before 21 ships
  does **not** bump again. Update hardcoded protocol expectations and manual protocol fixtures in tests
  with the bump.
- **Changelog.** `docs/next/CHANGELOG.md` takes user-facing runtime changes only. Stage 2 is internal and
  gets none.
- **Refactor risk.** Stages 3, 4 and 6 touch state, identity or persisted snapshots. Classify them as
  refactor-risk: name the protected behaviour, add or name characterization tests before moving code,
  use `AppState::assert_invariants_for_test()` with `AppState::test_with_adversarial_identity_state()`,
  and run a roundtable.
- **Code conventions.** No `unwrap()` in production code, `tracing` for logging, `#[allow]` only with a
  reason, platform-specific behaviour compile-gated into `src/platform/`.
- **Commits.** Lowercase conventional, no emoji, no AI co-author line, `refs #<n>` only when a real issue
  exists.

## Stage 2: label provenance

Internal. No user-visible change. Lowest risk, highest leverage: it is what deletes hwt's ledger, its
adoption heuristic and its mirror label guard.

**Files.** `src/workspace.rs` (`custom_name` at :182, `set_custom_name` at :1092),
`src/persist/snapshot.rs` (`WorkspaceSnapshot` at :50), `src/api/schema/workspaces.rs` (`WorkspaceInfo`
at :52), `src/app/api/workspaces.rs`, `src/app/api/plugins/mod.rs` (context payload).

1. `LabelOrigin { Derived, User, Rule { source } }` beside `custom_name`.
2. Audit every writer of a workspace label and record an origin. The TUI rename dialog is `User`. A
   naming rule is `Rule { source }`. herdr's own computation is `Derived`. **`worktree create --label`
   is `User`**: it is an explicit label from a caller with no source, and the asymmetry decides it -
   over-protecting a label costs a missing chip, under-protecting costs a rename that can reach another
   machine.
3. Persist `label_origin` on `WorkspaceSnapshot` behind `#[serde(default)]`.
4. Load migration: an absent field compares the stored label against
   `automatic_workspace_label(cwd, repo_root)` (`src/workspace/git/discovery.rs:63`) exactly once, equal
   giving `Derived` and anything else `User`, and records the answer. Never re-run.
   **Do not reimplement the comparison as a basename check.** The function returns `basename(repo_root)`,
   not `basename(cwd)`, and falls back to a cwd-derived label only when `repo_root` has no filename;
   a hand-rolled basename test misclassifies source checkouts and embedded bare repos. Calling it is
   also what makes the answer exact rather than a heuristic, since hwt could only guess from outside.
   **Do not reach for a null check on `custom_name` either.** `src/workspace.rs:256` stores the computed
   automatic label into `custom_name` at construction, so every existing workspace has a non-null value
   and null-ness carries no information.
5. Expose `label_origin` on `WorkspaceInfo` and in `HERDR_PLUGIN_CONTEXT_JSON`.
6. Nothing reads it to gate a write yet. Stage 6 does.

**Wire.** Additive fields only; compatible for JSON API consumers. No bump.

**Acceptance.** A workspace renamed in the TUI reports `User`. A fresh worktree workspace reports
`Derived`. Both survive a restart. A `session.json` written before this loads through the heuristic once
and a second load does not move it.

**Tests.** `AppState::test_new()`, snapshot round-trip, migration idempotence.

## Stage 3: worktree state classification

**Files.** `src/worktree.rs` (pure classifier next to the existing git command construction and parsing,
both already PTY-free), `src/api/schema/worktrees.rs`, `src/app/api/worktrees.rs`, `src/cli/worktree.rs`.

1. `WorktreeState` with the spec's seven variants, serialized snake_case. It is a wire format, so a
   variant serializes as its own value.
2. Pure `classify(...) -> WorktreeState` taking git-side facts plus the workspace side. No `App`, no PTY.
3. `WorktreeInfo` gains `state` and `path` becomes `Option<String>`, carrying
   `#[serde(skip_serializing_if = "Option::is_none")]`. That attribute is a requirement, not a style
   choice: `WorktreeInfo` is not only the `worktree list` row. It is also in the event payload
   (`src/events.rs:48`) and the plugin context payload (`src/app/api/plugins/mod.rs`), and hwt parses
   the latter on its `--from-context` path. Skipping `None` keeps `path` present for every entry that
   has one, so the three surfaces stay readable and the checkout-less entries simply fail hwt's join key
   the way an absent entry does today. Without it, Stage 3 breaks hwt while hwt is still in use.
4. Synthesize `orphan_workspace` entries (recorded membership whose checkout is absent) inline.
   `branch_only` entries are opt-in behind a list param (D7).
5. `foreign` keeps the submodule and bare detection: a submodule reports `path` as
   `.git/modules/<name>` with `is_linked_worktree: true`, so the join key never matches and a naive
   teardown would run `git worktree remove` on a git-internal directory.
6. `ambiguous` covers two workspaces claiming one checkout, and one workspace whose panes sit in
   different checkouts (herdr supports per-pane cwd; the mirror uses exactly that).
7. `herdr worktree status <path|branch|workspace>` in `src/cli/worktree.rs`, JSON out, rc 0/1/2.

**Wire.** `path` optional is incompatible. **Bump `PROTOCOL_VERSION` 20 to 21 here, once.** Regenerate
the API schema artifact. Changelog entry.

**Acceptance.** Table-driven classification including the submodule shape. `status` honours the rc
contract. The default `worktree list` call runs no `git for-each-ref`.

**Risk.** The production consumers of `WorktreeInfo` are `src/app/api/worktrees.rs`, the event payload
in `src/events.rs` and the plugin context payload in `src/app/api/plugins/mod.rs`. The TUI is not one of
them: `src/app/worktrees.rs` builds a `WorktreeInfo` only inside its `#[cfg(test)]` module and uses
`WorktreeSpaceMembership` in production. Characterization tests belong on the three API surfaces, not on
the dialogs.

## Stage 4: staged teardown

The destructive stage. Everything above it is preparation.

**Files.** `src/api/schema/worktrees.rs` (`WorktreeRemoveParams`, today `{ workspace_id, force }` and
unable to target a `closed` checkout at all), `src/app/api/worktrees.rs` and
`src/app/api/worktrees/deferred.rs`, `src/worktree.rs`, `src/cli/worktree.rs`.

1. Params gain `path`, `branch`, `keep_branch`, `delete_branch`, `dry_run`; `workspace_id` becomes
   optional and exactly one target is required.
2. Stages, each refusing on its own evidence: classify and stop on `foreign` or `ambiguous` with rc 2 and
   an empty `stages` list; workspace removal; a re-query closing any workspace that remains; branch
   deletion; provenance record cleanup.
3. `delete_branch` defaults false (D4). `git branch -d` normally, `-D` only under `force`.
4. The prunable arm computes the collateral list, every other prunable registration in that repo that a
   repo-wide `git worktree prune` will also drop, and puts it **in the response**. git has no per-entry
   prune, so this is reported and never prevented. `dry_run` is the only gate.
5. `create` failure cleanup: close an orphan workspace, and report a branch left behind by name without
   deleting it. The failure response carries nothing that distinguishes a branch the call created from
   one that already existed.
6. The rc contract on the new arms only; existing arms keep their behaviour so nothing regresses for
   current users.

**Wire.** Additive params plus optional `workspace_id`. If Stage 3 already bumped to 21 and 21 has not
shipped, do not bump again. Regenerate the schema artifact. Changelog entry.

**Acceptance.** Teardown of an `open` bench leaves no workspace, no checkout, and with `delete_branch` no
branch. `dry_run` mutates nothing. A `foreign` target is refused with nothing destroyed. The collateral
list appears in the API response, not only in CLI text.

**Tests.** Per stage, plus an adversarial identity-state pass, plus the interaction with the existing
`dirty_worktree_requires_force` path.

**Risks, stated at the API and not only in prose.** `is_prunable` means "the directory is not visible
right now", not "the checkout is gone": a flapping mount makes a live worktree read prunable, after which
`git branch -D` succeeds against it. `git branch -d` is not a landed-work proof either - it tests
containment in the configured upstream or in the current HEAD, so a squash-merged branch is refused
correctly but for the wrong reason. Anything needing a real proof supplies its own.

## Stage 5: path rules

**Files.** `src/config/model.rs` (`WorktreesConfig` at :835, today a single `directory` string),
`src/config/io.rs`, `src/worktree.rs`, `src/app/worktrees.rs` (dialog preview),
`docs/next/website/src/data/config-reference.json`.

1. `[[worktrees.path]]` with `when_match` (regex over the branch leaf) and `template`. `directory` stays
   as the fallback and its default `~/.herdr/worktrees` does not move.
2. Template variables are limited to what exists at creation time: `{repo}`, `{branch}`,
   `{branch_leaf}`, `{task}`, `{login}`, `{host}`. Not `{leaf}` - there is no checkout yet, and a
   predicate over the directory leaf cannot be evaluated at all.
3. Every rule falling through is a refusal demanding an explicit path. The existing argument-less create
   keeps its generated name for compatibility; the rule path never reaches it.
4. The TUI New worktree dialog shows the resolved path before creating, computed from the same
   resolution the create will use, so preview and result cannot diverge.

**Repo.** `[[worktrees.path]]` is an array of tables; confirm whether `config_reference_check` wants a
`SKIPPED_SUBTREES` entry or per-key rows. Config docs edits need ja and zh-cn parity. Changelog entry.

**Acceptance.** Two branch shapes land in two different trees, which is the one thing
`[worktrees] directory` cannot express and the popup's entire reason to exist. A fall-through refuses
rather than inventing a name.

## Stage 6: naming

Highest risk of the set, and the only thing here that can leave the machine.

**Files.** config parsing beside `src/config/sidebar.rs`, `src/workspace.rs`, `src/metadata_tokens.rs`,
plus the event wiring in `src/app/`.

1. `[worktrees.naming]` carrying `label`, `label_source`, `[vars.*]` and `[tokens.*]`.
2. Derivation runs per workspace at server start and on structural events (worktree created or removed,
   workspace created, membership resolved). **Never in a render or layout loop.**
3. Provenance gate: write only where `label_origin` is `Derived` or this rule's own
   `Rule { source }`. Never over `User`.
4. Unresolved handling, each rule paid for by a measurement in the spec: an unresolved variable in a
   label template throws the whole label away and writes nothing; an unresolved `{linked}` means the
   template cannot be chosen so nothing is written; a declared token takes `on_unresolved` as `clear`
   (default), `omit`, or a literal, because omission is not deletion and a finished worker's chip
   otherwise stays forever; an undeclared key is someone else's and is untouched.
5. Display discrimination is structural (`leaf == branch_leaf`, case-insensitive), not a shape regex. A
   shape regex alone reads `orbit-3-way-merge` as issue `ORBIT-3`.
6. Token ownership per D3: declared keys are reserved and a `report-metadata` write to one is refused
   with a named error. `MAX_SEQUENCE_SOURCES` stays 32.
7. **Derivation is off by default (D6).** With no `[worktrees.naming]` block herdr derives nothing, so
   herdr never needs to know that herdr-mirror exists. This replaces hwt's mirror guard rather than
   porting it. The hazard is real and belongs at the config key's documentation: the mirror reads a local
   label change as operator intent and pushes `workspace.rename` to the remote on a 60 s poll, so on a
   machine running the mirror, enabling derivation before the mirror reads `label_origin` can rename a
   workspace on another machine.
8. Chips are re-derived at start, not persisted (D5). A restart briefly shows no chips.

**Perf.** If any part lands in a pane-scaled path, profile fixed geometry at 1 and at least 15 populated
panes with `just bench-render-scale` and report the scaling delta.

**Repo.** Config reference rows for every key. Changelog entry. Doc page edits need ja and zh-cn parity.

**Acceptance.** A configured derivation stamps a fresh bench and refuses a human-renamed one. `clear`
removes a stale chip. A restart re-derives. With no config block, nothing is written anywhere.

## Stage 7: attn preset

Smallest and fully independent. Good first landing to exercise the whole pipeline.

**Files.** `src/app/agent_view.rs`, the agent view schema, `src/ui/sidebar.rs` (`AgentPanelSort` at :83),
`src/config/keybinds.rs`.

1. A built-in preset filtering agent status to `blocked` or `done`, installed through the existing
   declarative `agent_view` filter and sort machinery.
2. Empty-set guard: with nothing matching, do not install the view, notify instead. Expanded, herdr draws
   `no matching agents` itself; collapsed, `render_sidebar_collapsed` draws neither that text nor the view
   label, so a filtered-empty sidebar and a dead herdr look identical.
3. The client cycles `grouped` to `priority` to `attn`, with a keybind toggle.
4. Counting distinguishes "I could not look" from "nothing needs you": a response that parses and carries
   no error is not yet a response of the expected shape.

hwt's per-server state file, its socket identity check and its source-matched clear probe all go. The
ambiguity they resolve is an artifact of not being the server.

## Stage 8: retire hwt

Not herdr-repo work. Unlink the plugin, drop the CLI symlink, and leave Linear and orbit's
`node`/`tool`/`role` chips where they belong, outside herdr.

## Sequencing

- Independent: 2, 3, 5, 7.
- 4 depends on 3 (it refuses on classification).
- 6 depends on 2 (the provenance gate) and 5 (the shared template engine).
- 8 depends on all.

Recommended order: **7, 2, 3, 4, 5, 6, 8**. Stage 7 is small, additive and touches nothing destructive,
so it proves the contribution pipeline end to end before anything expensive rides on it.

## Rollback

One branch and one PR per stage against the fork. Stages 2, 5 and 7 are additive and revert cleanly.
Stage 3 carries the protocol bump, so reverting it must revert the bump unless a later stage already
shipped on 21. Stage 4 changes destructive behaviour and should be exercised on a throwaway repo and a
throwaway session before it is pointed at anything that matters.

## Definition of done, per stage

- `just check` green.
- API schema artifact regenerated when schema types changed.
- Config reference updated when config keys changed.
- Changelog entry for user-facing changes only.
- Characterization tests named and passing on the refactor-risk stages.
- No `unwrap()` in production code, no placeholder branches, no skipped tests standing in for work.
