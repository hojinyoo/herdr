# Migrating hwt into herdr

`hwt` (`herdr-worktree`) is an external CLI and herdr plugin that manages a git worktree and its herdr
workspace as one unit. It exists because `herdr worktree` creates and removes the two together but does
not manage them to the end: teardown leaves the branch, a failed create leaves the branch, and an
argument-less create invents a name and makes a real worktree.

This spec covers moving that functionality into herdr itself. Source of truth for what hwt does today:
`~/project/orbit/tools/herdr-worktree/docs/design.md` (1,533 lines, every claim measured) and its README.
Measured against herdr 0.7.5/0.8.0; this worktree is 0.8.2.

## What is being migrated

4,194 lines of Python across 16 modules, plus 7,447 lines of tests and a live drill.

```
hwt [--session s] [--config c] [--repo p] [--json] <command>
  ls     [--all]                                          inventory + verdicts
  status <path|branch|wsid> | --from-context               one bench in full
  new    --branch n [--base r] [--path p] [--task t] [--label l] [--fetch] [--issue id]
  open   <target> | --from-context [--label l] [--issue id]
  rm     <target> | --from-context [--dry-run] [--force] [--keep-branch] [--confirm] [--issue done|release]
  name   <target> | --all | --from-context | --from-event [--apply] [--adopt] [--include-focused] [--take-over]
  forget --from-event
  attn
```

Plus a plugin manifest: five actions, one `worktree.removed` event hook, one popup pane that reads a
branch name and previews the path before creating.

## The central claim

Most of hwt is boundary tax. It is large because it is outside herdr, not because the problem is large.

hwt joins three CLI surfaces (`worktree list`, `workspace list`, `pane list`) that herdr holds as one
`AppState`. It classifies silence, because a successful `report-metadata` answers zero bytes and a
successful mutation cannot be told from a no-op. It runs a protocol compatibility gate, because the
client and server can be different builds. It canonicalizes paths at a boundary where herdr mixes
caller spelling with git spelling in the same response. It keeps a ledger at
`~/.local/state/herdr-worktree/labels.json` because `workspace list` returns the label as a bare string
with no provenance, so a human rename, herdr's derivation and hwt's own last write are
indistinguishable.

In-process none of that exists. What survives the move is the domain modelling: the state table, the
staged teardown, the declarative naming rules, and the refusal contract.

## Disposition

| hwt module | LOC | Disposition |
|---|---|---|
| `wire` | 305 | Drop. In-process calls return `Result`; there is no silence to classify and no protocol skew against yourself. |
| `inventory` | 515 | Drop the join and the repo discovery. Keep path canonicalization at the `git worktree list` boundary, where `src/worktree.rs` already sits. |
| `verdict` | 157 | Keep seven of eight states. `unknown` meant "a read failed"; in-process a read does not half-fail. |
| `ops` | 755 | Keep the staged teardown and the create failure cleanup. Drop the prove-by-re-query wrapper around every step. |
| `config` | 416 | Keep, ported to herdr's config. |
| `naming` | 637 | Keep. Server side. |
| `ledger` | 211 | Drop. Replaced by a provenance field on the workspace. |
| `guard` | 157 | Drop the mirror refusals (provenance replaces the guessing). Keep glyph validation on the naming config. |
| `context` | 101 | Drop. Inside herdr the target is an id. |
| `cli` | 311 | Keep the 0/1/2 rc contract on the herdr worktree subcommands. |
| `popup` | 150 | Fold into herdr's native New worktree dialog as a path preview. |
| `attn` | 201 | Shrink to an `agent_view` preset. herdr already owns declarative filter/sort. |
| `linear` | 235 | Out of scope. Stays in the orchestrator. |
| `paths`, shims | 43 | Drop. |

## Architecture

### No new noun

hwt calls the (worktree, workspace) pair a *bench*. herdr does not need a third noun beside workspace
and worktree: `worktree list` already returns the pair through `open_workspace_id`. The state goes on
`WorktreeInfo` as a field.

`WorktreeInfo.path` is required today, and two states have no checkout (`orphan_workspace`,
`branch_only`). Recommendation: make `path` optional and keep one array, since callers already switch
on the sibling booleans. This is an incompatible wire change; see protocol below.

The seven states, unchanged in meaning from hwt §2:

| State | Worktree | Workspace | Destruction |
|---|---|---|---|
| `open` | present | open | yes |
| `closed` | present | none | yes, git path |
| `prunable` | directory gone | - | yes, prune |
| `orphan_workspace` | none | open | workspace only |
| `branch_only` | none | none | branch only |
| `foreign` | - | - | no. Submodule or bare, join key mismatch |
| `ambiguous` | 1 | 2+ | no |

`foreign` stays: a submodule reports `path` as `.git/modules/<name>` with `is_linked_worktree: true`,
so the join key never matches and a naive teardown runs `git worktree remove` on a git-internal
directory.

### Label provenance is the keystone

Add `label_origin` to the workspace and persist it in `WorkspaceSnapshot`:

```
Derived            herdr computed it, through automatic_workspace_label
User               a human renamed it
Rule { source }    a naming rule wrote it
```

This one field deletes the ledger, the adoption heuristic, the `custom_name` ambiguity and the mirror
label guard:

- Adoption today is "current label == `basename(checkout_path)`", which is hwt reimplementing a
  derivation it cannot call. herdr's own is `automatic_workspace_label(cwd, repo_root)`
  (`src/workspace/git/discovery.rs:63`): `basename(repo_root)`, falling back to a cwd-derived label only
  when `repo_root` has no filename. For a linked worktree `repo_root` is that worktree's own root, which
  is why the two usually agree; for a source checkout or an embedded bare repo they do not. Inside herdr
  the comparison is exact because the function is callable, which also retires hwt's open question about
  the derived label drifting off the basename.
- `custom_name` cannot serve, and the source says why hwt's measurement came out as it did:
  `src/workspace.rs:256` stores the computed automatic label **into** `custom_name` at construction. So
  every existing workspace has a non-null `custom_name`, null-ness proves nothing in either direction,
  and `session.json` lags a rename by at least one generation on top of that.
- The mirror renames real workspaces on other machines off a local label change
  (`mirror.rs:301` `resolve_label`, pushed at `:780`, on a 60 s poll). hwt refuses to write any label
  that renders into a mirror prefix shape, derived from `~/.config/herdr-mirror/hosts.toml`, failing
  closed when that file is unreadable. Provenance lets the mirror ask instead. Until the mirror reads
  it, derived labels must still not be written to workspaces the mirror owns.

Migration for existing sessions: the field is absent, so `#[serde(default)]` compares the stored label
against a freshly computed `automatic_workspace_label` exactly once at load and records the answer. Same
question hwt asks on every pass, asked once and answered exactly.

### Tokens do not persist

`WorkspaceSnapshot` has ten fields and none of them is `tokens`. Labels survive a restart, chips do not.
If derivation moves into herdr, the server re-derives at start rather than persisting: same code path,
cannot go stale, and it removes hwt's whole re-stamp trigger problem (the `worktree.created` hook it
already deleted, and the unverified `[[startup]]` firing condition).

Cost: a restart briefly shows no chips until derivation runs.

### Server and client split

Per the runtime/client guardrail:

- Server: state classification, path rules, staged teardown, label and token derivation, `label_origin`,
  agent view presets.
- Client: the dialog's path preview, sidebar chip rendering, the attn keybind cycle.

Every new fact goes on the JSON API. Nothing new goes through the private TUI client socket.

### Staged teardown

Extend `worktree remove`, do not add a verb. `WorktreeRemoveParams` is `{ workspace_id, force }` today
and cannot target a `closed` checkout at all.

```
0. classify → refuse on foreign / ambiguous, rc 2, nothing destroyed
1. workspace open? → worktree remove --workspace <id>   else → git worktree remove <path>
2. re-query: a workspace still there → workspace close
3. git branch -d (-D under force)
4. drop the provenance record
```

New params: `path` / `branch` targeting, `keep_branch`, `dry_run`, `delete_branch`.

**Branch deletion must be opt-in.** hwt's default is to delete; herdr's current behaviour is to leave it.
Flipping herdr's default silently destroys branches for existing users.

**The prunable arm cannot be made safe, only honest.** Stage 1 for a `prunable` entry is a repo-wide
`git worktree prune`, and git has no per-entry prune. Every other prunable registration in that repo
goes with it. The collateral list belongs in the API response, not only in CLI text, and `dry_run` is
the only gate. `is_prunable` means "the directory is not visible right now": a flapping mount (NFS
`$HOME`) makes a live worktree read prunable, after which `git branch -D` succeeds against it. The
boundary is that `-d` refuses an unmerged branch and `-D` needs an explicit force.

**`git branch -d` is not a landed-work proof.** It tests containment in the configured upstream or in
the current HEAD, never whether the work landed. A squash-merged branch is refused, correctly but for
the wrong reason. Anything that needs a real proof supplies its own.

### The rc contract

herdr answers rc 1 for every failure today, so `protocol_mismatch` and a missing socket are the same
number. The new arms need the three-value contract or they re-create the hazard hwt exists to avoid:

| rc | Meaning |
|---|---|
| 0 | success |
| 1 | proven absent, no-op, proceed |
| 2 | unproven. Stop. Nothing was destroyed |

### Path rules replace the single directory

`[worktrees] directory` is one string, so a path that depends on the branch shape is not expressible.
That gap is the popup's entire reason to exist.

```toml
[worktrees]
directory = "~/.herdr/worktrees"          # unchanged default, used when no rule matches

[[worktrees.path]]
when_match = '^[A-Za-z][A-Za-z0-9]*-[0-9]+-'   # over the branch leaf, at creation time only
template   = "~/worktrees/{repo}/{branch_leaf}"

[[worktrees.path]]
template   = "~/worktrees/{repo}-{task}"
```

`when_match` reads `{branch_leaf}` and nothing else. At creation time there is no checkout and no pane,
so a predicate over the directory leaf cannot be evaluated. All rules falling through is a refusal that
demands an explicit path, never an invented name. herdr's existing argument-less create keeps working;
the rule path never reaches it.

### Naming

```toml
[worktrees.naming]
label        = "{repo}/{branch_leaf}"
label_source = "{repo}"

[worktrees.naming.vars.issue]
from               = "branch_leaf"
match              = '^([A-Za-z][A-Za-z0-9]*-[0-9]+)-'
group              = 1
case               = "upper"
require_leaf_match = true

[worktrees.naming.tokens.wt]
from = "linked"
map  = { true = "⑂", false = "⌂" }
```

Rules carried over from hwt §5, each of which cost a measurement:

- An unresolved variable in a label template throws the whole label away and writes nothing. On a
  detached HEAD `{branch_leaf}` is unresolved and the label becomes `orbit/`; herdr accepts an empty
  label and the automatic one is gone irreversibly once `custom_name` is set.
- An unresolved `{linked}` means the label template cannot even be chosen, so nothing is written.
- Omission is not deletion. A declared token left out keeps its previous value, so a finished worker's
  chip stays forever. Default `on_unresolved` is `clear`, with `omit` and a literal as alternatives.
- An undeclared key is someone else's and is never touched.
- Display-time discrimination is structural, not a shape regex: issue-native means
  `leaf == branch_leaf` case-insensitively. A shape regex alone reads `orbit-3-way-merge` as issue
  `ORBIT-3`. Measured 8/8 on the real fleet.
- A parse result never reaches a destruction gate. Its blast radius is one chip.

**Open decision: token ownership.** Keys merge into one flat map with no attribution, and `--source` is
used for sequence tracking only, so any source can overwrite or clear any key. hwt lives with a `hwt_`
prefix convention enforced in its own code. Inside herdr the derived tokens are herdr's, so the honest
options are (a) reserve declared keys and refuse a `report-metadata` write to them with a named error,
or (b) keep derived values in a separate map and merge at render with a stated winner. Recommend (a):
it fails loudly at the write instead of silently at the render. `MAX_SEQUENCE_SOURCES` is 32 and needs
no change.

**Perf.** Derivation runs per workspace on structural events and at server start, never in a render or
layout loop. If any part lands in one, profile fixed geometry at 1 and 15+ populated panes with
`just bench-render-scale`.

### attn

`agent_view` already takes a declarative filter and sort with a source and a label, so attn is a
built-in preset filtering status to `blocked` or `done`. Two behaviours carry over:

- With nothing to show it does not turn on, it notifies instead. Expanded, herdr draws
  `no matching agents` itself; collapsed, `render_sidebar_collapsed` draws neither that text nor the
  view label, so a filtered-empty sidebar and a dead herdr look identical.
- Counting must distinguish "I could not look" from "nothing needs you". A response that parses and
  carries no error is not yet a response of the expected shape.

The client cycles `grouped → priority → attn`. hwt's per-server state file, its socket identity check
and its source-matched clear probe all go: the ambiguity they resolve is an artifact of not being the
server.

## Stages

Each stage ships alone. hwt keeps working until the last one. Files touched, per-stage acceptance
criteria and the repo obligations each one carries are in
[hwt-migration-plan.md](hwt-migration-plan.md).

1. **This spec.**
2. **Label provenance.** `label_origin` on the workspace, persisted, exposed on `workspace list` and in
   the plugin context payload. No user-visible change. Lowest risk, highest leverage, and hwt's ledger
   becomes redundant without being removed.
3. **State classification.** The seven states on `worktree list`, `path` optional, orphan and
   branch-only entries synthesized. `herdr worktree status <target>`. Wire change.
4. **Staged teardown.** Path and branch targeting, `keep_branch`, `dry_run`, `delete_branch`, collateral
   reporting, the 0/1/2 contract on the new arms. Create failure cleanup: close an orphan workspace,
   report a branch left behind and never delete it (the failure response cannot tell a branch it created
   from one that already existed).
5. **Path rules.** `[[worktrees.path]]` and the dialog preview.
6. **Naming.** Label and token derivation, gated by `label_origin`, re-derived at start and on
   structural events.
7. **attn preset.**
8. **Retire hwt.** What remains in Python is Linear and orbit's own chips; those stay outside.

Stages 3 and 4 change the wire. Compare `src/protocol/wire.rs::PROTOCOL_VERSION` against what stable
and preview have published, bump once if the current source protocol is already out, and update the
hardcoded protocol expectations and manual fixtures in tests.

## Risks

1. **Upstream acceptance, before Stage 2.** This is a fork branch. herdrdev/herdr auto-closes
   unsolicited implementation PRs and nobody asked for this feature. Either an invitation exists, or
   this is a fork-local carry with a rebase cost on every release. Decide first; every later stage
   compounds it.
2. **Naming is the only thing here that can reach another machine.** Provenance makes it safe in
   principle, but only once the mirror reads it. Until then Stage 6 must keep hwt's refusal: no derived
   label on a workspace the mirror owns, detected by the `.mirror-pane` marker in a pane's `cwd` or
   `foreground_cwd`, and by the prefix shape derived from `hosts.toml` failing closed.
3. **Prune collateral and `-D`** are described above and are not fixable by us. They must be visible in
   the API, not buried in CLI prose.
4. **Default-flip on branch deletion** destroys branches for existing users if it goes in silently.
5. **Orchestrator intent has no representation.** Orbit's `.agent/pinned` lives inside the directory
   being torn down and herdr reads nothing under `.agent/`. If orbit keeps its own GC, herdr's verdict
   stays advisory to it and never the authority.
6. **Refactor risk.** Stages 3, 4 and 6 touch state, identity and persisted snapshots. Classify as
   refactor-risk, name the protected behaviour and add characterization tests before moving code, and
   run a roundtable.
7. **Chips vanish across a restart** under the re-derive choice. Accepted; persisting them instead trades
   a gap for staleness.

## Out of scope

- Linear, or any issue tracker. hwt writes state and labels only when `--issue` names the target on the
  command line and `[linear]` names the repo's profile; that authority belongs to the orchestrator, not
  to a terminal runtime.
- The mirror's own guards. Provenance is what herdr owes the mirror. The rest is the mirror's.
- Orbit's `node` / `tool` / `role` chips. `role` is the conjunction of two conditions, one of which is
  not derivable from herdr and git.
- Auto-recovery (hwt §11.2) and mirror bench display (§11.1). Designed, never built, and neither is a
  prerequisite.
- A `command = [...]` escape hatch in naming. Measured need is zero; the file hatch covers orchestrator
  state at one stat plus one read per workspace.

## Tests

- State classification is a pure function over an inventory struct. Table-driven, no PTY,
  `AppState::test_new()`.
- Provenance migration: a snapshot with no `label_origin` loads through the heuristic once, and a second
  load does not re-run it.
- Teardown: characterization tests named before any code moves; identity and state work uses
  `assert_invariants_for_test()` with `test_with_adversarial_identity_state()`.
- Path rules: template rendering, `when_match` over the branch leaf, and fall-through refusal.
- Naming: the derivation table, `on_unresolved` in all three forms, and the provenance gate refusing a
  `User` label.
- Do not port the drill. It exists to prove response shapes and silence across a process boundary that
  will not be there.
