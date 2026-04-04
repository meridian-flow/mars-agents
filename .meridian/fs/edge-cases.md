# Edge Cases

Per-stage catalog of edge cases, failure modes, and non-obvious behavior. Read [architecture.md](architecture.md) first for the normal flow.

## Resolver (`src/resolve/mod.rs`)

### Cycles

Kahn's algorithm in `topological_sort()` detects cycles. After BFS drains, any unvisited nodes form the cycle. Error: `ResolutionError::Cycle { chain }` where `chain` joins unvisited node names with ` → `.

No explicit self-dependency check — a source listing itself in its manifest's `[dependencies]` would create a cycle caught by the same codepath. The `_self` name is reserved at the config level (rejected during effective config construction), not in the resolver.

### Duplicate SourceId

Two different dependency names resolving to the same `SourceId` (same git URL or same canonical path) → `ResolutionError::DuplicateSourceIdentity`. Checked during BFS via `id_index: HashMap<SourceId, SourceName>`.

Conversely, the same name used with different `SourceId` values → `ResolutionError::SourceIdentityMismatch`. This catches config errors where a transitive dep's URL diverges from the direct dep's URL for the same name.

### Version Parsing and `v`-prefix Handling

`parse_version_constraint()` in `src/resolve/mod.rs:117-160`:

| Input | Parsed as |
|---|---|
| `None`, `""`, `"latest"` | `VersionConstraint::Latest` |
| `"v1.2.3"` | Exact match: `=1.2.3` |
| `"v2"` | Major range: `>=2.0.0, <3.0.0` |
| `"v2.1"` | Minor range: `>=2.1.0, <2.2.0` |
| `">=0.5.0"`, `"^2.0"`, `"~1.2"` | Semver requirement (passed to `semver::VersionReq::parse`) |
| anything else | `VersionConstraint::RefPin(string)` — treated as branch/commit ref |

Non-semver tags (e.g., `"nightly"`, `"release-candidate"`) fall through to `RefPin` and are fetched as git refs, not version-selected.

Pre-release versions: the `semver` crate handles pre-release matching per the semver spec. Mars does not add custom pre-release logic. A constraint like `^1.0.0` will not match `1.0.0-rc.1` unless the constraint explicitly includes pre-release.

Tags listed by `VersionLister` must parse as semver (`tag.strip_prefix('v') → semver::Version::parse`). Tags that don't parse are silently excluded from version selection. An untagged repo (empty `available` list) falls through to HEAD fetch.

### Lock Replay

When a lock file exists and `--frozen` is not set:

1. `select_version()` checks if the locked version satisfies all constraints → reuses it (skip refetch)
2. `fetch_git_version()` / `fetch_git_ref()` receive `preferred_commit` from the lock
3. If the locked commit is unreachable (tag force-pushed): `MarsError::LockedCommitUnreachable` → warns and re-resolves without preferred commit
4. With `--frozen`: unreachable commit becomes a hard error

"Constraint changed" means: the version in the lock no longer satisfies the intersected constraints from all dependents. The resolver doesn't try lock replay in that case — it proceeds to fresh version selection.

### Latest Constraint Behavior

`VersionConstraint::Latest` makes the resolver pick the newest available version (same as maximize mode for that source), regardless of the global MVS setting. This is intentional — `@latest` means "I want the newest, always."

### No Available Versions

When `list_versions()` returns empty (no semver tags):
- Default: fetch HEAD with locked commit as `preferred_commit`
- `mars upgrade`: fetch HEAD without preferred commit (force fresh)
- `--frozen` with unreachable locked commit: hard error

## Target Builder (`src/sync/target.rs`, `src/discover/mod.rs`)

### Discovery Rules

`discover_source()` in `src/discover/mod.rs`:

| Convention | Discovered as |
|---|---|
| `agents/*.md` | `ItemKind::Agent` — non-recursive, `.md` extension required |
| `skills/*/SKILL.md` | `ItemKind::Skill` — directory must contain `SKILL.md` |
| Root `SKILL.md` (no agents/ or skills/ found) | Flat skill — entire repo is one skill, `source_path = "."` |
| Hidden files/dirs (`.`-prefixed) | Skipped |
| Non-`.md` files in `agents/` | Skipped |
| Subdirectories in `agents/` | Skipped (non-recursive) |
| `skills/` entries without `SKILL.md` | Skipped |

**Flat skill precedence:** If both `skills/foo/SKILL.md` and root `SKILL.md` exist, the nested structure wins — root `SKILL.md` is only used as a fallback when no conventional items are discovered.

**Flat skill naming:** Uses `fallback_name` parameter (source name), or directory name if no fallback, or `"unknown-skill"` as last resort.

### Directory Hashing

`hash::compute_dir_hash()` in `src/hash/mod.rs`:

1. Walk all files recursively (follows directory structure, not symlinks to directories)
2. Collect `(relative_path, sha256_of_file_content)` pairs
3. **Sort lexicographically by path** — deterministic regardless of filesystem ordering
4. Concatenate as `"path:hash\n"` strings
5. SHA-256 of the concatenated manifest

**Path normalization:** Backslashes replaced with forward slashes for cross-platform determinism.

**Symlinks:** `file_type.is_dir()` check uses `entry.file_type()` which does NOT follow symlinks for the type check, but `fs::read()` for content hashing DOES follow symlinks. A symlink to a file is hashed by its target content. A symlink to a directory is not recursed into (it's not `is_dir()` per the entry metadata).

**File modes:** Not included in the hash. Only file paths and content matter.

**Flat skill exclusions:** `FLAT_SKILL_EXCLUDED_TOP_LEVEL` in `src/fs/mod.rs`: `.git`, `.mars`, `mars.toml`, `mars.lock`, `mars.local.toml`, `.gitignore`. Applied at the top level only — nested files with these names are included.

### Collision Rename Derivation

`extract_owner_repo_from_id()` in `src/sync/target.rs`:

- **Git source:** Parses `owner/repo` from the URL (strips protocol, `.git` suffix, extracts last two path components). Result: `owner_repo` with `/` replaced by `_`.
- **Path source:** Uses the dependency name directly as the suffix.
- **Format:** `{original_name}__{suffix}` → e.g., `coder__haowjy_meridian-base`
- **Applied to ALL colliding items**, not just one side — both items get renamed.

### Frontmatter Rewriting

When skills are auto-renamed and agents reference the old name in `skills:` frontmatter:

1. Build a map: `original_skill_name → [(new_name, source_name)]`
2. For each agent, determine which renamed variant to use:
   - Prefer the variant from the agent's own source
   - Fall back to the variant from a dependency of the agent's source
3. Parse frontmatter via `frontmatter::rewrite_content_skills()` — **exact string match** replacement in the `skills:` list. Substrings are safe: renaming `"plan"` does not affect `"planner"` or `"planning-extended"`.
4. Rewritten content stored in `target_item.rewritten_content` — applied during install.

**Parser contract:** `src/frontmatter/mod.rs` uses `serde_yaml` to parse the YAML block between `---` delimiters. Keys are accessed via `Value::String` lookup. The `skills` field must be a YAML sequence of strings. Flow style (`skills: [a, b]`) and block style are both handled. Non-mapping frontmatter returns `FrontmatterError::NotAMapping`. Missing or malformed frontmatter is tolerated — the item is still discovered and installed without rewriting.

### Unmanaged Collision Handling

`check_unmanaged_collisions()` in `src/sync/target.rs`:

A target item whose `dest_path` exists on disk but is NOT in the lock file → unmanaged collision. The item is **removed from `target_state`** before diffing, preserving the user's file. Warning printed.

**Exception:** If the disk content hash matches `target_item.source_hash`, it's treated as a partial prior install (crash recovery) and allowed through.

## Diff Engine (`src/sync/diff.rs`)

### Disk Hash Computation

`hash::compute_hash()` dispatches on `ItemKind`:
- `Agent` → `hash_bytes(fs::read(path))` — single file SHA-256
- `Skill` → `compute_dir_hash(path)` — deterministic directory hash (see above)

### File ↔ Directory Transitions

Not explicitly handled. If an agent path becomes a skill directory (or vice versa) between versions, the behavior depends on the lock entry:

- The old lock entry (e.g., `agents/foo.md`) becomes an orphan (removed)
- The new target item (e.g., `skills/foo`) becomes an `Add` (installed fresh)
- No migration logic — the old file is deleted and the new directory is created

This works because items are keyed by `dest_path`, and `agents/foo.md` ≠ `skills/foo`.

### Orphan Semantics

An item in the lock but not in the target → `DiffEntry::Orphan`. Causes:

- **Dependency removed** from `mars.toml`
- **Filter changed** — item was included before, now excluded
- **Rename changed** — item had old dest_path, now has new one
- **Item removed upstream** — source no longer contains the agent/skill

All produce the same `Orphan` → `Remove` outcome. Mars does not distinguish the cause.

### `--force` vs Checksum Semantics

Normal mode:
- `local_changed?` = disk hash ≠ `installed_checksum`
- Captures user edits since mars last wrote the file

Force mode:
- `local_changed?` = disk hash ≠ `source_checksum`
- Effect: any file whose disk content differs from the **original source** (not mars's rewritten version) is treated as having local changes. Since force converts `LocalModified` to `Overwrite`, the practical effect is: everything gets overwritten to match the source.

The subtle case: a file that was frontmatter-rewritten by mars (so `installed_checksum ≠ source_checksum`) and then NOT edited by the user. In normal mode: `Unchanged`. In force mode: disk hash matches `installed_checksum` but not `source_checksum` → `LocalModified` → `Overwrite`. This means `--force` re-applies the frontmatter rewriting from scratch.

### Deleted Files

If a file tracked in the lock has been deleted from disk:
- `disk_path.exists()` is false → `local_changed = None` (treated as "not locally changed")
- If source unchanged: becomes `Add` (reinstall), not `Unchanged`
- If source changed: becomes `Update` (clean overwrite)

## Apply (`src/sync/apply.rs`)

### Action Ordering

Actions are processed in the order they appear in `SyncPlan.actions`, which mirrors the diff entry order:
1. Target items (in `IndexMap` insertion order — topological source order × discovery order)
2. Orphans (lock items not in target)
3. `_self` symlinks (injected by `sync::execute()`)
4. Stale `_self` removals

There is **no explicit ordering guarantee** between installs and removes. In practice, removes happen after target items because orphans are appended after the target loop in `diff::compute()`.

### Crash Consistency

Mars does NOT have transactional rollback across multiple actions. If the process dies mid-apply:

- **Completed actions** have already modified disk and cache. Their effects persist.
- **Pending actions** haven't run. The lock file hasn't been written yet (step 17 in `sync::execute()`).
- **On next sync:** The old lock is still authoritative. Completed-but-unlocked installs appear as unmanaged files → `check_unmanaged_collisions()` detects them. If content matches source hash, they're treated as crash recovery and allowed through. If content differs (partial write via atomic_write shouldn't happen, but if it does), they're reported as unmanaged collisions and skipped.

Individual file operations ARE crash-safe:
- `atomic_write()`: tmp+rename is atomic on POSIX. Partial write → temp file, not destination.
- `atomic_install_dir()`: rename-old-then-rename-new minimizes the window. Old content preserved as `.{name}.old` during the swap. Stale `.old` cleaned up on next run.

### Three-Way Merge Details

**Implementation:** `src/merge/mod.rs` wraps `git merge-file -p` via subprocess.

**Inputs:**
- `base`: cached content from `.mars/cache/bases/{installed_checksum}` — what mars wrote last time
- `local`: current disk content (`root.join(local_path)`)
- `theirs`: new source content (with frontmatter rewrites applied if any)

**Behavior:**
- Clean merge (exit 0): merged content installed, `ActionTaken::Merged`
- Conflicts (exit >0): content with git-standard conflict markers installed, `ActionTaken::Conflicted`
- Error (exit <0): `MarsError::Source` — typically `git` not in PATH

**Labels in conflict markers:**
```
<<<<<<< local
user's modification
======= 
>>>>>>> {source_name}@upstream
```

**Skills merge limitation:** Three-way merge reads `SKILL.md` as the merge target for skill directories. Per-file merge within a skill directory is not supported — the entire directory is replaced on update.

### Base Cache Population

`cache_base_content()` in `src/sync/apply.rs`:

- Called after every `Install` and `Overwrite` action
- Content-addressed by `installed_checksum` — path is `cache_bases_dir.join(checksum)`
- **Immutable:** if cache file already exists, skip write
- **Agents:** cache the full file content
- **Skills:** cache only `SKILL.md` content (the merge-relevant part)
- **Missing cache:** merge degrades to empty base → two-way diff → more conflict markers, never crashes

### `_self` Items

When `mars.toml` has `[package]`, the project's own agents/skills are symlinked:

1. `discover_local_items()` finds items in the project root
2. Collision check: `_self` items shadow external items (warning printed, external removed from plan)
3. Symlinks use **relative paths** computed via `pathdiff::diff_paths()`
4. Stale `_self` entries (in old lock but not in current project) are pruned
5. If `[package]` is removed entirely, all `_self` entries are cleaned up
6. Unmanaged collision check applies to `_self` items too — if a non-locked file exists at the symlink target, it's skipped with a warning
