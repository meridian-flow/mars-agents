# Sync Pipeline

Every mutating command (`add`, `remove`, `sync`, `upgrade`, `override`, `rename`) runs the same pipeline. The pipeline resolves the full desired state before touching any files.

## Pipeline Overview

The pipeline is implemented as typed phase functions in `src/sync/mod.rs`. Each phase consumes the prior phase's output struct by value (move semantics, no cloning).

```
mars.toml + mars.local.toml
        │
        ▼
┌─────────────────┐
│  1. Load Config │  Acquire sync lock, load config, apply mutations, merge effective config
└────────┬────────┘
         ▼
┌─────────────────┐
│  2. Resolve     │  Fetch sources, discover transitive deps, merge model aliases
└────────┬────────┘
         ▼
┌─────────────────┐
│  3. Build Target│  Discover items, apply filters, detect collisions
└────────┬────────┘
         ▼
┌─────────────────┐
│  4. Create Plan │  Diff target vs lock + disk → Add/Update/Conflict/Orphan → actions
└────────┬────────┘
         ▼
┌─────────────────┐
│  5. Apply Plan  │  Write resolved content to .mars/ canonical store (atomic writes)
└────────┬────────┘
         ▼
┌─────────────────┐
│  6. Sync Targets│  Copy from .mars/ to each configured target directory
└────────┬────────┘
         ▼
┌─────────────────┐
│  7. Finalize    │  Write lock, persist dep model aliases, build report
└─────────────────┘
```

### Phase handoff structs

| Phase | Struct | Key contents |
|---|---|---|
| 1 | `LoadedConfig` | `Config`, `LocalConfig`, `EffectiveConfig`, `old_lock`, `_sync_lock` |
| 2 | `ResolvedState` | `LoadedConfig` + `ResolvedGraph` + `model_aliases` |
| 3 | `TargetedState` | `ResolvedState` + `TargetState` + renames + validation warnings |
| 4 | `PlannedState` | `TargetedState` + `SyncPlan` |
| 5 | `AppliedState` | `PlannedState` + `ApplyResult` |
| 6 | `SyncedState` | `AppliedState` + `Vec<TargetSyncOutcome>` |

A `DiagnosticCollector` is threaded through all phases. No `eprintln!` in library code — all warnings/info go through structured diagnostics.

## Step Details

### 1. Load Config (`load_config`)

Acquires `.mars/sync.lock` via advisory file locking, then loads `mars.toml` and `mars.local.toml`. If the command includes a mutation and `mars.toml` doesn't exist, an empty config is created (auto-init for `mars add` on a fresh project).

Under the sync lock, applies the command's mutation atomically:

| Mutation | Source |
|---|---|
| `UpsertDependency` | `mars add` |
| `BatchUpsert` | `mars add source1 source2` |
| `RemoveDependency` | `mars remove` |
| `SetOverride` / `ClearOverride` | `mars override` |
| `SetRename` | `mars rename` |

For `UpsertDependency`, filter replacement is atomic: if any filter field is present in the new entry, the entire filter config replaces the existing one. If no filter fields are set (e.g., version bump only), existing filters are preserved.

Then merges `mars.toml` with `mars.local.toml` overrides into `EffectiveConfig`. For each dependency:

- Validates `url` XOR `path` (exactly one required)
- Validates filter combinations (see [configuration.md](configuration.md#filter-mode-rules))
- Applies local overrides (path replaces URL, preserves original git spec)
- Computes `SourceId` for each dependency (git URL or canonical path)
- Rejects `_self` as a dependency name (`_self` is reserved for local package items from the current project)

### 2. Resolve (`resolve_graph`)

Fetches sources and resolves concrete versions.

**Algorithm (src/resolve/mod.rs):**
1. Fetch dependencies from EffectiveConfig
2. Read `mars.toml` manifests in source trees to discover transitive dependencies (including transitive skill dependencies pulled in through agent declarations)
3. Intersect version constraints across dependents
4. Select concrete versions
5. Topological sort (Kahn's algorithm: deps before dependents)

**Version selection strategy:**

| Mode | Strategy | Used by |
|---|---|---|
| Normal | Minimum Version Selection (MVS) | `mars sync`, `mars add` |
| Maximize | Newest compatible version | `mars upgrade` |

**MVS** picks the minimum version satisfying all constraints. This is deterministic and conservative: you get exactly what you asked for, not the newest thing available. `mars upgrade` switches to maximize mode to find the newest compatible version.

**Lock replay:** When a lock file exists, the resolver tries to reuse locked commits for sources whose version constraints haven't changed. This makes `mars sync` fast and deterministic after the first install. In `--frozen` mode, lock replay failures become hard errors (the lock must fully reproduce the previous state).

**Source types:**

| Source | Resolution |
|---|---|
| Git with version constraint | List tags → filter by semver constraint → select version → fetch tree |
| Git without version | Fetch HEAD (default branch tip) |
| Git with ref pin | Fetch the specific branch/commit ref |
| Local path | Resolve to canonical path, no version logic |

Additionally, this phase merges model aliases from the dependency tree. Each resolved dependency's `[models]` config is collected in **declaration order** (the order deps appear in the consumer's `mars.toml`, not alphabetical). `merge_model_config()` applies two layers: dependencies first (declaration-order first-wins on sibling conflicts), consumer config on top (always wins). Within transitive subtrees, each parent's manifest declaration order determines its children's ordering. Diamond deps inherit the position of the earliest direct dep that reaches them. See [configuration.md](configuration.md#merge-precedence) for the full precedence rules, conflict warnings, and examples.

### 3. Build Target (`build_target`)

Constructs the desired target state from the resolved graph.

For each source in topological order:
1. **Discover** items in the source tree (`agents/*.md`, `skills/*/SKILL.md`, flat `SKILL.md`)
2. **Apply filter** (All, Include, Exclude, OnlySkills, OnlyAgents)
3. **Apply rename** mappings from config
4. **Compute source hash** (SHA-256 of source content)

After building all items:
5. **Detect naming collisions** — items from different sources with the same destination path
6. **Auto-rename collisions** — suffix with `__{owner}_{repo}` derived from source URL/name
7. **Rewrite frontmatter** — update skill references in agents to match renamed skill names (`frontmatter` is the YAML metadata block at the top of each agent Markdown file)
8. **Check unmanaged collisions** — items that would overwrite files not tracked in the lock

### 4. Create Plan (`create_plan`)

Computes diff and converts to executable actions.

Compares target state against the lock file and disk to produce diff entries.

Uses dual checksums from the lock:
- `source_checksum`: what the source provided (before any rewriting)
- `installed_checksum`: what mars wrote to disk (after frontmatter rewriting)

The diff matrix:

| Source changed? | Local changed? | Result |
|---|---|---|
| No | No | **Unchanged** (skip) |
| Yes | No | **Update** (clean overwrite) |
| No | Yes | **LocalModified** (keep local) |
| Yes | Yes | **Conflict** → source wins overwrite + warning |
| — | — | **Add** (new item) |
| — | — | **Orphan** (in lock but not in target → remove) |

With `--force`, the baseline for "local changed" shifts to `source_checksum`, so conflicted files are treated as local modifications and get overwritten.

Also injects local package items when the project has a `[package]` section — the project's own agents/skills are added to the target state under the `_self` source name (`_self` is the reserved local-project source identifier).

### 5. Apply Plan (`apply_plan`)

Executes planned actions against the `.mars/` canonical store:

| Action | Behavior |
|---|---|
| Install | Atomic write (tmp + rename) or atomic directory install |
| Update | Replace with new source content |
| Overwrite | Replace with source content (conflicts: source wins) |
| Remove | Delete file or directory |
| Note | `_self` items follow the same Install path as dependency items |
| Skip / KeepLocal | No-op, recorded in outcomes |

In `--diff` (dry run) mode, actions are computed but not executed.

### 6. Sync Targets (`sync_targets`)

Copies content from `.mars/` canonical store to each configured target directory (`.agents/`, `.claude/`, etc.). Implemented in `src/target_sync/mod.rs`.

- Targets include the managed root (default: `.agents/`) plus any additional directories added via `mars link` (`settings.targets`)
- All targets get file copies
- Uses `reconcile::fs_ops` for atomic operations (tmp+rename)
- Orphan cleanup: uses the previous lock to identify mars-managed files in each target, removes only those that are no longer in the current apply outcomes
- Non-fatal per-target: errors on one target are recorded in `TargetSyncOutcome` but don't stop other targets from syncing

### 7. Finalize (`finalize`)

Writes lock and constructs the final `SyncReport`.

- **Lock write**: constructs new `mars.lock` from resolved graph + apply outcomes (checksums). Keys sorted deterministically for clean git diffs. Lock is written **regardless of target sync outcome** — this ensures the lock always reflects what's in `.mars/`, even if a target sync failed.
- **Model aliases**: persists dependency-only model aliases to `.mars/models-merged.json`. Uses an empty consumer map so the cache contains only dep-sourced aliases — `mars models list` can then overlay the current consumer config at read time without stale consumer aliases from prior syncs.
- **Validation warnings**: emits diagnostics for missing skill references in agents.
- **Report**: assembles `SyncReport` with apply outcomes, target sync outcomes, diagnostics, and dry-run flag.

## Local Package Items (`_self`)

When `mars.toml` has a `[package]` section, the project's own agents and skills are discovered, hashed, and installed into the managed root via the normal sync pipeline — the same install/copy path as dependency items.

- `_self` items shadow external items if names collide (with a warning)
- Removed local items are cleaned up on the next `mars sync`
- If `[package]` is removed, all `_self` entries are cleaned up
