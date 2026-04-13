# Development Guide: mars-agents

Mars is an agent package manager. It installs agent profiles and skills from git/local sources into a `.mars/` canonical store, then copies managed content into target directories (`.agents/`, `.claude/`, `.cursor/`, etc.).

## Critical Invariants

### Mars does NOT own target directories

`.agents/`, `.claude/`, `.codex/`, `.cursor/` can contain hand-written content that mars didn't create. These are shared directories — mars writes into them but doesn't own them.

**Mars must NEVER delete files it didn't create.** Orphan cleanup must only remove files whose `dest_path` was in the previous `mars.lock` but not in the current sync result. Never scan-and-delete-unknown.

The lock file (`mars.lock`) is the authority on what mars manages. If it's not in the lock, mars doesn't touch it.

### Atomic writes

Config, lock, and installed files use temp + rename. Crash mid-write leaves the old file intact. All target copies use the `reconcile` layer's atomic operations (`src/reconcile/fs_ops.rs`).

### Resolve first, then act

Validate + build full desired state before mutating files. If any conflict or error is detected during resolution, zero mutations occur.

### No heuristics

User intent is expressed through explicit flags and arguments, not inferred from string patterns.

### No builtin model aliases

The mars binary contains zero hardcoded model aliases. All aliases come from packages (via `[models]` in their `mars.toml`) or from the consumer's own `mars.toml`. This keeps the binary distribution-neutral and lets packages control the alias namespace.

## Architecture

```
mars.toml + mars.lock (committed, project root)
        ↓ mars sync
    .mars/ (canonical store, gitignored)
        ↓ copy to each target
    targets: .agents/, .claude/, .cursor/ (committed, shared)
```

- **`.mars/`** — canonical content store. Gitignored. Rebuilt by `mars sync` from lock + sources. Contains resolved content (`agents/`, `skills/`), merge base cache (`cache/bases/`), models cache (`models-cache.json`), dependency model aliases (`models-merged.json`), sync lock.
- **Target directories** — mars copies managed items into these from `.mars/`. They may contain non-mars content. Mars tracks what it put there via the lock. Configured via `settings.targets` in mars.toml, defaults to `[".agents"]`.
- **`.mars/` is NOT the source of truth** — it's a cache. The committed targets + `mars.lock` are the authority. On fresh clone (no `.mars/`), `mars sync` rebuilds the cache from sources.

## Sync Pipeline

Orchestrated in `src/sync/mod.rs` with typed phase functions (move semantics — each phase consumes the prior phase's output struct, no cloning):

```
load_config → resolve_graph → build_target → create_plan → apply_plan → sync_targets → finalize
```

1. **load_config** — acquire sync lock (advisory file locking: Unix `flock`, Windows `LockFileEx`), load `mars.toml` + local overrides, apply mutations, build `EffectiveConfig`, load existing lock
2. **resolve_graph** — resolve dependency versions, merge model aliases from dependency tree (consumer > deps, declaration order)
3. **build_target** — discover items (agents/skills) from all sources including local package, detect collisions, apply filters, rewrite frontmatter refs
4. **create_plan** — diff desired state against lock + disk, generate sync plan (Add/Update/Conflict/Orphan)
5. **apply_plan** — write resolved content to `.mars/` canonical store (atomic writes via tmp+rename)
6. **sync_targets** — copy from `.mars/` to each configured target directory (atomic copies, never deletes files mars didn't create, non-fatal per-target — errors don't stop other targets)
7. **finalize** — write lock (regardless of target sync outcome), persist dependency-only model aliases to `models-merged.json`, validation warnings, build `SyncReport`

### Phase structs

Each phase produces a typed handoff struct consumed by the next phase:

| Phase | Struct | Key contents |
|---|---|---|
| 1 | `LoadedConfig` | `Config`, `LocalConfig`, `EffectiveConfig`, `old_lock`, `_sync_lock` |
| 2 | `ResolvedState` | `LoadedConfig` + `ResolvedGraph` + `model_aliases` |
| 3 | `TargetedState` | `ResolvedState` + `TargetState` + renames + validation warnings |
| 4 | `PlannedState` | `TargetedState` + `SyncPlan` |
| 5 | `AppliedState` | `PlannedState` + `ApplyResult` |
| 6 | `SyncedState` | `AppliedState` + `Vec<TargetSyncOutcome>` |

### Key types

- `SourceOrigin::Dependency(name)` / `SourceOrigin::LocalPackage` — where an item came from
- `InstallDep` — consumer install intent (`mars.toml [dependencies]`)
- `ManifestDep` — package export (required URL, no path deps)
- `_self` local package items are added as regular `TargetItem` entries during `build_target` (no special materialization — same install/copy path as dependency items)
- `DiagnosticCollector` — threaded through entire pipeline, collects structured warnings/info

## Model Catalog

`src/models/mod.rs` — model aliases, catalog caching, pattern resolution.

### No builtins

All model aliases come from packages or consumer config. The binary ships zero hardcoded aliases. Merge precedence: consumer > dependencies (declaration order, first-dep wins).

### Two alias modes

- **Pinned**: `model = "claude-opus-4-6"` — explicit ID, no resolution needed
- **AutoResolve**: `provider = "Anthropic"`, `match = ["opus"]`, `exclude = ["thinking"]` — glob matching against cached model catalog, newest release date wins

### Catalog lifecycle

- `mars models refresh` — fetches from models.dev API, caches to `.mars/models-cache.json`
- `mars models list` — loads dependency aliases from `.mars/models-merged.json`, overlays consumer config from `mars.toml [models]`, then applies visibility filtering from `[settings.model_visibility]` (unless overridden by `--include`/`--exclude`)
- `mars models resolve <alias>` — resolves against cache

### Dependency model merge

During `resolve_graph`, model configs from all resolved dependencies are collected in declaration order. `merge_model_config()` layers them: deps first (first-dep wins on conflicts), consumer config on top (always wins). Result is used during sync and persisted to `models-merged.json` in `finalize()` as dependency-only aliases (no consumer config baked in, so `models list` can overlay fresh consumer config at read time).

## Managed Targets

`src/target_sync/mod.rs` — copies content from `.mars/` canonical store to configured target directories.

- Targets configured via `settings.targets` in `mars.toml` (default: `[".agents"]`)
- All targets get file copies — no symlinks anywhere in the pipeline
- Orphan cleanup uses the previous lock to identify mars-managed files, only removes those
- Non-fatal per-target: errors on one target don't stop other targets from syncing
- Target sync runs after `apply_plan` and before `finalize` — lock is written regardless of target sync outcome

## Reconciliation Layer

`src/reconcile/` — shared atomic filesystem operations.

- **Layer 1 (`fs_ops`)**: `atomic_write_file`, `atomic_copy_file`, `atomic_copy_dir`, `atomic_install_dir` — all use tmp+rename pattern, file permissions 0o644
- **Layer 2 (`reconcile_one`)**: state-based reconciliation — `scan_destination()` → compare with `DesiredState` → create/update/remove/skip/conflict

## Structured Diagnostics

`src/diagnostic.rs` — `DiagnosticCollector` threaded through the entire pipeline.

- No `eprintln!` in library code — all warnings/info go through the collector
- Machine-readable codes (e.g. `"shadow-collision"`, `"model-alias-conflict"`, `"target-sync-error"`)
- CLI layer formats diagnostics for human or JSON output
- Diagnostics are non-fatal — pipeline continues, report includes all collected diagnostics

## Key Modules

| Module | Responsibility |
|---|---|
| `src/sync/` | Pipeline orchestration + typed phase functions |
| `src/target_sync/` | Copy from `.mars/` to managed target directories |
| `src/reconcile/` | Shared atomic fs operations (Layer 1) + state-based reconciliation (Layer 2) |
| `src/models/` | Model catalog, auto-resolve, cache, no builtins |
| `src/config/` | `mars.toml` + `mars.local.toml` schemas, load/save, merge to `EffectiveConfig` |
| `src/lock/` | `mars.lock` schema, load/write, ownership tracking |
| `src/resolve/` | Dependency + version resolution and graph ordering |
| `src/source/` | Source fetching (git + path) and global cache |
| `src/discover/` | Discover agents/skills by filesystem conventions |
| `src/diagnostic.rs` | Structured diagnostics (no `eprintln!` in library code) |
| `src/cli/` | Clap args, root discovery, command dispatch, output formatting |
| `src/frontmatter/` | YAML frontmatter parsing and rewriting |
| `src/merge/` | Text merge utilities (conflict markers supported, but sync uses source-wins overwrite — `PlannedAction::Merge` is never produced by current plan logic) |
| `src/validate/` | Post-sync validation (e.g. missing skill references) |
| `src/fs/` | Low-level filesystem utilities, `FileLock`, atomic write primitives |

## Dev Workflow

```bash
cargo build
cargo test
cargo clippy
```

Integration tests under `tests/`. Prefer keeping changes localized to one module.

## Releasing

Two ways to release:

1. **CLI** (recommended):
   ```bash
   mars version patch --push    # bump, commit, tag, push → triggers CI
   ```

2. **Shell script** (runs full checks):
   ```bash
   scripts/release.sh patch --push    # fmt, clippy, tests, build, release
   ```

The `v*` tag triggers GitHub Actions to build and publish to PyPI, npm, and crates.io.
