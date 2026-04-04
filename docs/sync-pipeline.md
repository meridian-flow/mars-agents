# Sync Pipeline

Every mutating command (`add`, `remove`, `sync`, `upgrade`, `override`, `rename`) runs the same pipeline. The pipeline resolves the full desired state before touching any files.

## Pipeline Overview

```
mars.toml + mars.local.toml
        │
        ▼
┌─────────────────┐
│  1. Sync Lock   │  Acquire .mars/sync.lock (fcntl flock)
└────────┬────────┘
         ▼
┌─────────────────┐
│  2. Load Config │  Load mars.toml + mars.local.toml
└────────┬────────┘
         ▼
┌─────────────────┐
│  3. Mutate      │  Apply ConfigMutation (UpsertDependency, Remove, etc.)
└────────┬────────┘
         ▼
┌─────────────────┐
│  4. Merge       │  Build EffectiveConfig (config + overrides → resolved deps)
└────────┬────────┘
         ▼
┌─────────────────┐
│  5. Resolve     │  Fetch sources, discover transitive deps, select versions
└────────┬────────┘
         ▼
┌─────────────────┐
│  6. Target      │  Build desired state: discover items, apply filters, detect collisions
└────────┬────────┘
         ▼
┌─────────────────┐
│  7. Diff        │  Compare target vs lock + disk → Add/Update/Conflict/Orphan
└────────┬────────┘
         ▼
┌─────────────────┐
│  8. Plan        │  Convert diff entries into executable actions
└────────┬────────┘
         ▼
┌─────────────────┐
│  9. Apply       │  Execute actions: install, update, merge, remove, symlink
└────────┬────────┘
         ▼
┌─────────────────┐
│ 10. Lock Build  │  Build new mars.lock from graph + apply outcomes
└─────────────────┘
```

## Step Details

### 1. Sync Lock

All sync operations acquire `.mars/sync.lock` via `fcntl` file locking. This prevents concurrent mars processes from racing on config reads and disk writes.

### 2. Load Config

Loads `mars.toml` and `mars.local.toml`. If the command includes a mutation and `mars.toml` doesn't exist, an empty config is created (auto-init for `mars add` on a fresh project).

### 3. Mutate Config

Applies the command's mutation atomically under the sync lock:

| Mutation | Source |
|---|---|
| `UpsertDependency` | `mars add` |
| `BatchUpsert` | `mars add source1 source2` |
| `RemoveDependency` | `mars remove` |
| `SetOverride` / `ClearOverride` | `mars override` |
| `SetRename` | `mars rename` |

For `UpsertDependency`, filter replacement is atomic: if any filter field is present in the new entry, the entire filter config replaces the existing one. If no filter fields are set (e.g., version bump only), existing filters are preserved.

### 4. Merge Effective Config

Merges `mars.toml` with `mars.local.toml` overrides into `EffectiveConfig`. For each dependency:

- Validates `url` XOR `path` (exactly one required)
- Validates filter combinations (see [configuration.md](configuration.md#filter-mode-rules))
- Applies local overrides (path replaces URL, preserves original git spec)
- Computes `SourceId` for each dependency (git URL or canonical path)
- Rejects `_self` as a dependency name (`_self` is reserved for local package items from the current project)

### 5. Resolve

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

### 6. Build Target State

Constructs the desired state of `.agents/` from the resolved graph.

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

### 7. Compute Diff

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
| Yes | Yes | **Conflict** (needs merge) |
| — | — | **Add** (new item) |
| — | — | **Orphan** (in lock but not in target → remove) |

With `--force`, the baseline for "local changed" shifts to `source_checksum`, so conflicted files are treated as local modifications and get overwritten.

### 8. Create Plan

Converts diff entries into executable actions: Install, Overwrite, Merge, Remove, Skip, KeepLocal, Symlink.

Also injects local package symlinks when the project has a `[package]` section — the project's own agents/skills are symlinked into the managed root under the `_self` source name (`_self` is the reserved local-project source identifier).

### 9. Apply

Executes planned actions against the filesystem:

| Action | Behavior |
|---|---|
| Install | Atomic write (tmp + rename) or atomic directory install |
| Update | Replace with new source content |
| Merge | Three-way merge using cached base version |
| Remove | Delete file or directory |
| Symlink | Create relative symlink for `_self` items |
| Skip / KeepLocal | No-op, recorded in outcomes |

In `--diff` (dry run) mode, actions are computed but not executed.

### 10. Build Lock

Constructs the new `mars.lock` from the resolved graph (source provenance) and apply outcomes (checksums). Keys are sorted deterministically for clean git diffs.

## Local Package Items (`_self`)

When `mars.toml` has a `[package]` section, the project's own agents and skills are symlinked into the managed root. This lets a source package test its own items without copying.

- `_self` items shadow external items if names collide (with a warning)
- Stale `_self` entries are pruned when items are removed from the local package
- If `[package]` is removed, all `_self` entries are cleaned up
