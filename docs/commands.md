# CLI Reference

All commands support two global flags:

| Flag | Description |
|---|---|
| `--root <PATH>` | Explicit managed root (default: auto-detect from cwd) |
| `--json` | Machine-readable JSON output |

The managed root's parent directory is the project root (where `mars.toml` and `mars.lock` live).

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | `sync`: unresolved conflicts present; `check`: validation errors found |
| `2` | Config, resolution, validation, lock, request, collision, or frozen-violation errors |
| `3` | I/O, source fetch, HTTP, or git CLI errors |

---

## `mars init`

Initialize a mars project. Creates `mars.toml` and the managed output directory.

```bash
mars init [TARGET] [--link DIR...]
```

| Argument/Flag | Description |
|---|---|
| `TARGET` | Directory name for managed output (default: `.agents`) |
| `--link DIR` | Directories to link after init (repeatable) |

**Behavior:**
- Creates `<project-root>/mars.toml` with `[dependencies]` section
- Creates the managed directory (default `.agents/`)
- Creates `.mars/` internal state directory
- Adds `mars.local.toml` and `.mars/` to `.gitignore`
- Persists non-default `managed_root` in `[settings]`
- Idempotent: re-running is a no-op for init, but still processes `--link`

```bash
mars init                    # Default: .agents/
mars init .claude            # Custom managed root
mars init --link .claude     # Init + link in one step
```

---

## `mars add`

Add one or more dependencies, then sync.

```bash
mars add <source...> [filter flags]
```

| Flag | Description |
|---|---|
| `--agents NAME,...` | Install only named agents (include mode) |
| `--skills NAME,...` | Install only named skills (include mode) |
| `--exclude NAME,...` | Exclude named items |
| `--only-skills` | Install only skills from this source |
| `--only-agents` | Install only agents + transitive skill dependencies (skills those agents declare in frontmatter) |

**Rules:**
- Filter flags require exactly one source. Multi-source add is only for whole-source installs.
- Re-adding an existing dependency updates it. Filter fields are replaced atomically when any filter flag is present; version-only changes preserve existing filters.
- Source specifiers: `owner/repo`, `owner/repo@version`, URLs, local paths.

Dependency naming model:
- `mars add` uses the source specifier to derive a dependency name (for example, `haowjy/meridian-base` -> `meridian-base`).
- That dependency name is the key stored in `mars.toml` under `[dependencies.<NAME>]`.
- Subsequent commands use that dependency name, not the source URL.

```bash
mars add haowjy/meridian-base
mars add haowjy/meridian-base@^1.0
mars add ../my-local-agents
mars add acme/ops --agents deployer,monitor
mars add acme/toolkit --only-skills
mars add source1 source2          # Multi-source (no filters)
```

---

## `mars remove`

Remove a dependency and prune its installed items.

```bash
mars remove <name>
```

Removes the named dependency from `mars.toml` and runs sync to clean up installed items. Whole-source removal only; to narrow a source's installed items, use `mars add` with updated filters.

---

## `mars sync`

Resolve dependencies and make the managed root match config.

```bash
mars sync [--force] [--diff] [--frozen]
```

| Flag | Description |
|---|---|
| `--force` | Overwrite local modifications for managed files |
| `--diff` | Dry run: show what would change without applying |
| `--frozen` | Error if lock file would change (CI mode) |

This is the core operation. It runs the full [sync pipeline](sync-pipeline.md): resolve → target → diff → apply.

---

## `mars upgrade`

Upgrade dependencies to newest versions within their constraints.

```bash
mars upgrade [names...]
```

| Argument | Description |
|---|---|
| `names` | Specific dependency names to upgrade (default: all) |

Uses maximize strategy instead of minimum version selection. Only git sources with semver constraints are upgradeable; path sources have no version to maximize.

```bash
mars upgrade                 # Upgrade all
mars upgrade base dev        # Upgrade specific deps
```

---

## `mars outdated`

Show available updates without applying them.

```bash
mars outdated
```

Displays a table with columns: SOURCE, LOCKED, CONSTRAINT, UPDATEABLE, LATEST.

- For tagged repos: compares locked version against available tags
- For untagged repos: compares locked commit against current HEAD
- Path sources are skipped (no version to check)

---

## `mars list`

List installed agents and skills.

```bash
mars list [--source NAME] [--kind KIND] [--status]
```

| Flag | Description |
|---|---|
| `--source NAME` | Filter by dependency name |
| `--kind KIND` | Filter by item kind (`agents` or `skills`) |
| `--status` | Show detailed status (source, version, hash check) |

Default view shows name + description from frontmatter (the YAML metadata block at the top of each Markdown profile). With `--status`, output includes the dependency source name, version, and integrity status (`ok`, `modified`, `missing`, `conflicted`).

---

## `mars why`

Explain why an item is installed.

```bash
mars why <name>
```

Shows the item's source dependency name, version, install path, and which agents depend on it (for skills). Matches by name stem (agents) or directory name (skills).

```bash
mars why frontend-design
# frontend-design (skill)
#   provided by: base@v1.2.0
#   installed at: skills/frontend-design
#   required by:
#     agents/frontend-coder.md
```

---

## `mars rename`

Rename a managed item by adding a rename mapping to its source config.

```bash
mars rename <from> <to>
```

| Argument | Description |
|---|---|
| `from` | Current item path (e.g., `agents/coder__haowjy_meridian-base.md`) |
| `to` | Desired item path (e.g., `agents/coder.md`) |

Adds a `rename` entry to the dependency's config in `mars.toml` and re-syncs. Useful for resolving auto-rename collisions with preferred names.

---

## `mars resolve`

Mark conflicts as resolved after manually editing conflicted files.

```bash
mars resolve [file]
```

| Argument | Description |
|---|---|
| `file` | Specific file to resolve (default: all conflicted items) |

Checks for remaining conflict markers (`<<<<<<<`, `>>>>>>>`). If the file is clean, updates the lock file's `installed_checksum` to match the current disk content.

---

## `mars override`

Set a local development override for a source.

```bash
mars override <name> --path <local-path>
```

Writes to `mars.local.toml` and re-syncs. The local path replaces the git URL for resolution. `<name>` is the dependency name from `mars.toml`. See [local-development.md](local-development.md).

---

## `mars link`

Symlink `agents/` and `skills/` into a tool directory.

```bash
mars link <target> [--unlink] [--force]
```

| Flag | Description |
|---|---|
| `--unlink` | Remove symlinks instead of creating them |
| `--force` | Replace whatever exists (data may be lost) |

**Behavior:**
- Creates `<target>/agents -> <managed-root>/agents` and `<target>/skills -> <managed-root>/skills`
- Conflict-aware: scans target before mutating. If conflicts exist, reports all problems and aborts (zero mutations)
- Mergeable directories: unique files in target are moved into managed root before linking
- Foreign symlinks (pointing elsewhere) are reported as conflicts unless `--force`
- Persists the link in `mars.toml [settings] links`
- Only removes symlinks owned by this mars root on `--unlink`

```bash
mars link .claude            # Link agents/ and skills/ into .claude/
mars link .cursor            # Link to another tool
mars link --unlink .claude   # Remove links
mars link .claude --force    # Replace whatever exists
```

---

## `mars check`

Validate a source package before publishing.

```bash
mars check [path]
```

| Argument | Description |
|---|---|
| `path` | Directory to validate (default: current directory) |

Does not require a mars project (no `mars.toml` needed). Validates:
- Package structure: `agents/*.md`, `skills/*/SKILL.md`, or flat `SKILL.md`
- Frontmatter: name, description presence and consistency
- Duplicate names across agents and skills
- Skill dependency references (warns about external deps)
- Symlinks in source packages (warned)
- Missing `SKILL.md` in skill directories

```bash
mars check                   # Check current directory
mars check ../my-agents      # Check another directory
```

---

## `mars doctor`

Diagnose problems in an installed mars project.

```bash
mars doctor
```

Checks:
- Config validity (parses `mars.toml`)
- Lock file integrity
- Each locked item: file exists on disk, no conflict markers, checksum computability
- Config-lock consistency: dependencies in config match lock entries
- Agent skill references: every declared skill dependency exists on disk
- Link health: symlinks exist, point to correct managed root, not broken

Exit code 0 = healthy, 2 = issues found. See [troubleshooting.md](troubleshooting.md).

---

## `mars repair`

Rebuild managed state from config + dependencies.

```bash
mars repair
```

Runs a forced sync that overwrites everything. If the lock file is corrupt, resets it to empty and rebuilds from scratch. Handles unmanaged collisions during corrupt-lock recovery by removing colliding paths and retrying (bounded to 1024 retries).

---

## `mars cache`

Manage the global source cache.

```bash
mars cache <info|clean>
```

### `mars cache info`

Show cache location and disk usage (total, archives, git clone cache).

- Prints cache path plus total size breakdown.
- Supports `--json` with `path`, `total_bytes`, `archives_bytes`, and `git_bytes`.

### `mars cache clean`

Remove cached source trees (archives + git clones).

- Removes cache contents while preserving the cache directory structure.
- Prints reclaimed bytes (total, archives, git) and supports `--json`.
