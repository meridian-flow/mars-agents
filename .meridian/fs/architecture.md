# Mars Architecture

Mars is a package manager for agent profiles and skills. It resolves versioned dependencies from git repos and local paths, installs them into a managed `.agents/` directory, tracks ownership via a lock file, and handles content conflicts with three-way merge.

## Module Map

```
src/
├── config/mod.rs       Config loading, merging, validation (mars.toml + mars.local.toml)
├── resolve/mod.rs      Dependency resolution: fetch, version select, topo sort
├── sync/
│   ├── mod.rs          Pipeline orchestrator: execute() runs the full sync
│   ├── target.rs       Build desired state from resolved graph
│   ├── diff.rs         Compare target vs lock+disk → diff entries
│   ├── plan.rs         Convert diff entries → executable actions
│   └── apply.rs        Execute actions against filesystem
├── lock/mod.rs         Lock file schema, load/save, build from outcomes
├── discover/mod.rs     Filesystem-convention item discovery
├── frontmatter/mod.rs  YAML frontmatter parse/rewrite for .md files
├── hash/mod.rs         SHA-256 hashing for files and directories
├── merge/mod.rs        Three-way merge via git merge-file
├── fs/mod.rs           Atomic file ops, directory install, flock
├── source/
│   ├── mod.rs          GlobalCache, ResolvedRef, AvailableVersion
│   ├── git.rs          Git fetch, tag listing, archive extraction
│   ├── parse.rs        URL parsing, owner/repo extraction
│   └── path.rs         Local path source resolution
├── validate/mod.rs     Post-sync skill reference validation
├── types.rs            Core newtypes: SourceName, ItemName, DestPath, SourceId, etc.
├── error.rs            Error hierarchy: MarsError, ConfigError, LockError, ResolutionError
├── cli/                CLI command handlers (one file per command)
├── lib.rs              Library re-exports
└── main.rs             CLI entrypoint
```

## Core Types

Defined in `src/types.rs` unless noted otherwise.

| Type | Definition | Purpose |
|---|---|---|
| `SourceName` | `String` newtype | Dependency name from `mars.toml` keys |
| `SourceId` | `enum { Git { url }, Path { canonical } }` | Stable identity for dedup across names |
| `ItemName` | `String` newtype | Agent or skill name (filename-derived) |
| `ItemKind` | `enum { Agent, Skill }` (`lock/mod.rs`) | Two item types mars manages |
| `ItemId` | `struct { kind: ItemKind, name: ItemName }` (`lock/mod.rs`) | Unique item identity |
| `DestPath` | `PathBuf` newtype | Relative path under `.agents/` |
| `ContentHash` | `String` newtype | `"sha256:<hex>"` format |
| `MarsContext` | `struct { project_root, managed_root }` | Resolved project paths |
| `SourceUrl` | `String` newtype | Git URL |
| `CommitHash` | `String` newtype | Git commit SHA |

## Pipeline Stages

Every mutating command (`add`, `remove`, `sync`, `upgrade`, `override`, `rename`) runs `sync::execute()` in `src/sync/mod.rs`. The pipeline:

```
Config → Mutate → Merge → Resolve → Target → Diff → Plan → Apply → Lock
```

### 1. Config Load & Mutate

**Module:** `src/config/mod.rs`, `src/sync/mod.rs`

| Input | Output |
|---|---|
| `mars.toml` + `mars.local.toml` on disk | `EffectiveConfig` (merged, validated) |

**Key types:**
- `Config` — top-level `mars.toml` struct: `dependencies: IndexMap<SourceName, DependencyEntry>`, `package: Option<PackageInfo>`, `settings: Settings`
- `DependencyEntry` — `url`, `path`, `version`, `filter: FilterConfig`
- `FilterConfig` — `agents`, `skills`, `exclude`, `rename`, `only_skills`, `only_agents`
- `LocalConfig` — `overrides: IndexMap<SourceName, OverrideEntry>` (path swaps)
- `EffectiveConfig` — merged view with `EffectiveDependency` entries containing resolved `SourceId`, `SourceSpec`, `FilterMode`, `RenameMap`
- `ConfigMutation` — enum: `UpsertDependency`, `BatchUpsert`, `RemoveDependency`, `SetOverride`, `ClearOverride`, `SetRename`

**Entry point:** `sync::execute()` steps 1-4b in `src/sync/mod.rs`.

**Invariants:**
- Sync lock (`.mars/sync.lock` via `fcntl flock`) held before any config read or mutation
- Each dependency must have exactly one of `url` XOR `path`
- `_self` is reserved — rejected as a dependency name
- Filter replacement is atomic: if any filter field is present in a mutation, the entire filter config replaces the old one; if no filter fields, existing filters are preserved

**Failure modes:**
- `ConfigError::NotFound` — missing `mars.toml` (tolerated when mutation creates it)
- `ConfigError::ConflictingFilters` — both `agents`/`skills` and `exclude` on same dep
- `ConfigError::Invalid` — `url` and `path` both present or both absent

### 2. Resolve

**Module:** `src/resolve/mod.rs`

| Input | Output |
|---|---|
| `EffectiveConfig`, `LockFile` (optional), `ResolveOptions` | `ResolvedGraph` |

**Key types:**
- `ResolvedGraph` — `nodes: IndexMap<SourceName, ResolvedNode>`, `order: Vec<SourceName>` (topological), `id_index: HashMap<SourceId, SourceName>`
- `ResolvedNode` — `source_id`, `resolved_ref: ResolvedRef`, `manifest: Option<Manifest>`, `deps: Vec<SourceName>`
- `ResolvedRef` (`source/mod.rs`) — `source_name`, `version: Option<semver::Version>`, `version_tag: Option<String>`, `commit: Option<CommitHash>`, `tree_path: PathBuf`
- `VersionConstraint` — `Semver(VersionReq)`, `Latest`, `RefPin(String)`
- `ResolveOptions` — `maximize: bool`, `upgrade_targets: HashSet<SourceName>`, `frozen: bool`

**Entry point:** `resolve::resolve()`

**Algorithm:**
1. Seed `VecDeque<PendingSource>` from direct deps
2. BFS: for each pending source, resolve via `SourceProvider` trait
3. Read `mars.toml` manifests in fetched trees → discover transitive deps → push to queue
4. Intersect version constraints across dependents
5. `select_version()`: MVS (minimum) or maximize (newest) from satisfying versions
6. `validate_all_constraints()`: post-hoc check that all constraints are satisfied
7. `topological_sort()`: Kahn's algorithm → `order` vector (deps before dependents)

**Version selection:**
- MVS (default): pick minimum satisfying version — deterministic, conservative
- Maximize (`mars upgrade`): pick newest satisfying version
- Lock replay: when locked version satisfies constraints, reuse it (avoids refetch)
- `--frozen`: lock replay failures become hard errors

**Traits (for testing):**
- `VersionLister` — `list_versions(url) → Vec<AvailableVersion>`
- `SourceFetcher` — `fetch_git_version()`, `fetch_git_ref()`, `fetch_path()`
- `ManifestReader` — `read_manifest(tree_path) → Option<Manifest>`
- `SourceProvider` — blanket impl for `T: VersionLister + SourceFetcher + ManifestReader`

**Failure modes:**
- `ResolutionError::VersionConflict` — no version satisfies all constraints
- `ResolutionError::DuplicateSourceIdentity` — two names resolve to same `SourceId`
- `ResolutionError::SourceIdentityMismatch` — same name resolves to different `SourceId`
- `ResolutionError::Cycle` — topological sort detects cycle
- `MarsError::LockedCommitUnreachable` — locked commit gone (tag force-push); warns and re-resolves unless `--frozen`

### 3. Build Target State

**Module:** `src/sync/target.rs`

| Input | Output |
|---|---|
| `ResolvedGraph`, `EffectiveConfig` | `(TargetState, Vec<RenameAction>)` |

**Key types:**
- `TargetState` — `items: IndexMap<DestPath, TargetItem>`
- `TargetItem` — `id: ItemId`, `source_name`, `source_id`, `source_path: PathBuf`, `dest_path: DestPath`, `source_hash: ContentHash`, `is_flat_skill: bool`, `rewritten_content: Option<String>`
- `RenameAction` — `original_name`, `new_name`, `source_name`

**Entry point:** `target::build_with_collisions()`

**Algorithm (4 phases):**
1. **Discover & collect**: For each source in topological order, call `discover::discover_source()`, apply filter, apply config renames, compute source hash → `Vec<TargetItem>`
2. **Detect collisions**: Group items by `dest_path`, find groups with >1 item
3. **Auto-rename collisions**: Suffix all colliding items with `__{owner}_{repo}` derived from `SourceId`
4. **Build TargetState**: Insert (possibly renamed) items into `IndexMap<DestPath, TargetItem>`

**Post-processing** (called by `sync::execute()`):
- `rewrite_skill_refs()` — when skills are renamed, rewrites `skills:` frontmatter in agents that reference them
- `check_unmanaged_collisions()` — items whose dest exists on disk but not in lock → skip installation, warn

**Invariants:**
- Discovery uses `src/discover/mod.rs` — agents: `agents/*.md`, skills: `skills/*/SKILL.md`, flat skill fallback: root `SKILL.md` when no conventional items found
- Items sorted by `(kind, name)` — `Agent < Skill`, then lexicographic
- Hash computation: agents use `hash::hash_bytes()` on file content; skills use `hash::compute_dir_hash()` on sorted `(relative_path, file_hash)` pairs; flat skills exclude `.git`, `.mars`, `mars.toml`, etc.

### 4. Compute Diff

**Module:** `src/sync/diff.rs`

| Input | Output |
|---|---|
| managed root, `LockFile`, `TargetState`, `force: bool` | `SyncDiff` |

**Key types:**
- `SyncDiff` — `items: Vec<DiffEntry>`
- `DiffEntry` — enum with 6 variants: `Add`, `Update`, `Unchanged`, `Conflict`, `Orphan`, `LocalModified`

**Entry point:** `diff::compute()`

**Algorithm:**
1. For each target item:
   - Look up in lock by `dest_path`
   - If not in lock → `Add`
   - If in lock: compare `source_hash` vs `locked.source_checksum` (source changed?) and disk hash vs `locked.installed_checksum` (local changed?)
   - Matrix: `(source_changed, local_changed)` → `Update` / `LocalModified` / `Conflict` / `Unchanged`
   - Special case: file deleted from disk but hashes match → `Add` (reinstall)
2. For each lock item not in target → `Orphan`

**`--force` semantics:** Baseline for "local changed" shifts from `installed_checksum` to `source_checksum`. Effect: conflicted files (whose disk content differs from the original source) become `LocalModified` and get overwritten.

**Invariants:**
- Disk hash computed via `hash::compute_hash()` — dispatches to file hash (agents) or dir hash (skills)
- Dual checksum design: `source_checksum` detects upstream changes, `installed_checksum` detects local edits. They differ when frontmatter rewriting occurred.

### 5. Create Plan

**Module:** `src/sync/plan.rs`

| Input | Output |
|---|---|
| `SyncDiff`, `SyncOptions`, `cache_bases_dir: Path` | `SyncPlan` |

**Key types:**
- `SyncPlan` — `actions: Vec<PlannedAction>`
- `PlannedAction` — enum: `Install`, `Overwrite`, `Skip`, `Merge`, `Remove`, `KeepLocal`, `Symlink`
- `SyncOptions` — `force: bool`, `dry_run: bool`, `frozen: bool`

**Entry point:** `plan::create()`

**Mapping:**
| DiffEntry | Normal | `--force` |
|---|---|---|
| `Add` | `Install` | `Install` |
| `Update` | `Overwrite` | `Overwrite` |
| `Unchanged` | `Skip` | `Skip` |
| `Conflict` | `Merge` (reads base from cache) | `Overwrite` |
| `Orphan` | `Remove` | `Remove` |
| `LocalModified` | `KeepLocal` | `Overwrite` |

**Post-processing** (in `sync::execute()`):
- `_self` symlinks injected for local package items
- Orphan-remove actions for `_self` items stripped (handled separately)
- Frozen gate: if any non-skip/non-keep actions exist and `--frozen`, error

**Merge base lookup:** `cache_bases_dir.join(locked.installed_checksum)` → if missing, empty vec (degrades to two-way diff).

### 6. Apply

**Module:** `src/sync/apply.rs`

| Input | Output |
|---|---|
| managed root, `SyncPlan`, `SyncOptions`, `cache_bases_dir` | `ApplyResult` |

**Key types:**
- `ApplyResult` — `outcomes: Vec<ActionOutcome>`
- `ActionOutcome` — `item_id`, `action: ActionTaken`, `dest_path`, `source_name`, `source_checksum`, `installed_checksum`
- `ActionTaken` — enum: `Installed`, `Updated`, `Merged`, `Conflicted`, `Removed`, `Skipped`, `Kept`, `Symlinked`

**Entry point:** `apply::execute()`

**Action implementations:**

| Action | Implementation |
|---|---|
| `Install` / `Overwrite` | `install_item()`: agents → `fs::atomic_write()`; skills → `fs::atomic_install_dir()` (or `_filtered` for flat skills). Then `cache_base_content()`. |
| `Merge` | Read source content, read local content, `merge::merge_content()` (wraps `git merge-file -p`), `fs::atomic_write()` result, cache merged content as new base. Returns `Merged` or `Conflicted` based on `merge_result.has_conflicts`. |
| `Remove` | `fs::remove_item()` — `remove_file` for agents, `remove_dir_all` for skills |
| `Symlink` | Remove existing dest, compute relative path, `std::os::unix::fs::symlink()` |
| `Skip` / `KeepLocal` | No-op, recorded in outcomes |

**Dry run:** `dry_run_action()` produces outcomes without touching disk. `installed_checksum` is `None` (can't compute without writing).

**Base cache:** Content-addressed by `installed_checksum`. Written after every install/overwrite. Missing cache degrades merge to empty base → more conflict markers, no crash.

### 7. Build Lock

**Module:** `src/lock/mod.rs`

| Input | Output |
|---|---|
| `ResolvedGraph`, `ApplyResult`, old `LockFile`, `_self` items | new `LockFile` |

**Key types:**
- `LockFile` — `version: u32`, `dependencies: IndexMap<SourceName, LockedSource>`, `items: IndexMap<DestPath, LockedItem>`
- `LockedSource` — `url?`, `path?`, `version?`, `commit?`, `tree_hash?` (reserved)
- `LockedItem` — `source`, `kind`, `version?`, `source_checksum`, `installed_checksum`, `dest_path`

**Entry point:** `lock::build()`

**Outcome mapping:**
| ActionTaken | Lock behavior |
|---|---|
| `Installed` / `Updated` / `Merged` / `Conflicted` | New entry with computed checksums |
| `Skipped` / `Kept` | Carried forward from old lock |
| `Removed` | Excluded |
| `Symlinked` | New entry with `source = "_self"` |

**Write:** `lock::write()` → `toml::to_string_pretty()` → `fs::atomic_write()`. Keys are sorted deterministically (IndexMap insertion order, ensured sorted during build).

## Filesystem Layout

```
project/
├── mars.toml                 Config: dependencies, filters, settings
├── mars.local.toml           Dev overrides (gitignored)
├── mars.lock                 Ownership registry (committed)
├── .mars/
│   ├── sync.lock             fcntl advisory lock file
│   └── cache/
│       └── bases/            Content-addressed merge base cache
│           └── sha256:...    Cached installed content by checksum
├── .agents/                  Managed output root (configurable)
│   ├── agents/               Agent .md files
│   └── skills/               Skill directories (each with SKILL.md)
~/.mars/
└── cache/
    ├── archives/             Extracted source trees
    └── git/                  Bare git clones for fetch
```

## Concurrency

All sync operations acquire `.mars/sync.lock` via `fcntl flock` (blocking) before reading config. The lock is held through completion. `FileLock` in `src/fs/mod.rs` wraps the fd — dropping the struct releases the lock. `try_acquire()` is available for non-blocking checks.

## Atomic Operations

- **File writes:** `fs::atomic_write()` — write to `NamedTempFile` in same directory, then `persist()` (rename). Same-filesystem guarantee for atomic POSIX rename.
- **Directory installs:** `fs::atomic_install_dir()` — copy to temp dir in same parent, rename old to `.{name}.old`, rename new into place, remove old. Stale `.old` from prior crashes cleaned up automatically.
- **Lock file:** Always written via `atomic_write()`.

## Error Hierarchy

`MarsError` in `src/error.rs` unifies all module errors. Exit codes:

| Code | Errors |
|---|---|
| 1 | Unresolved conflicts (`Conflict`) |
| 2 | Config, lock, resolution, validation, collision, frozen violation, unreachable commit |
| 3 | Source, I/O, HTTP, git CLI errors |
