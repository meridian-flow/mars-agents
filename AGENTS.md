# Development Guide: mars-agents

Mars is an agent package manager for `.agents/` directories. It installs agent profiles and skills from git/local sources, tracks ownership in `mars.lock`, and links managed content into tool directories like `.claude/`.

## Core Principles

1. **Resolve first, then act.** Validate + build full desired state before mutating files. If any conflict or error is detected during resolution, zero mutations occur.
   - *Why*: Partial failures leave users in states that are hard to diagnose. A user who sees "3 conflicts" can fix all three and retry. A user whose command half-completed has to figure out what happened first.

2. **Atomic writes.** Config, lock, and installed files use temp + rename. Crash mid-write leaves the old file intact.
   - *Why*: Mars manages files that agents and tools read continuously. A half-written mars.toml breaks every tool that reads it. tmp + rename is atomic on POSIX — no recovery logic needed.

3. **Lock file is authority.** `mars.lock` defines managed ownership and checksums. If it's not in the lock, it's not managed.
   - *Why*: Without a single authority, mars can't distinguish "user added this manually" from "mars installed this." The lock makes ownership explicit — safe coexistence with unmanaged files.

4. **No heuristics.** User intent is expressed through explicit flags and arguments, not inferred from string patterns.
   - *Why*: The dot-prefix heuristic (`starts_with('.')`) classified `./my-project` as a target dir — a real bug caught by reviewers. Explicit arguments are boring but predictable, and predictable tools earn trust.

## Managed Layout

```text
project/
  .agents/                   # managed root (default)
    mars.toml               # desired sources + settings
    mars.lock               # generated lock/ownership/provenance
    mars.local.toml         # local overrides (gitignored)
    .mars/                  # internal state (sync.lock, cache)
    agents/                 # installed agent profiles
    skills/                 # installed skills
  .claude/                   # optional linked tool dir
    agents -> ../.agents/agents
    skills -> ../.agents/skills
```

## MarsContext Invariant

`src/cli/mod.rs` defines:

- `MarsContext { managed_root, project_root }`
- Invariant enforced by `MarsContext::new`: managed root must be a subdirectory and therefore has a parent.
- `project_root` is always `managed_root.parent()` (canonicalized when possible).

This invariant is relied on by local-path resolution and link-target resolution.

## CLI Surface

### Global flags

| Flag | Meaning |
|---|---|
| `--root <PATH>` | Use this managed root instead of auto-discovery. |
| `--json` | Machine-readable output. |

### Commands

| Command | Purpose |
|---|---|
| `mars init [TARGET]` | Initialize managed root with `mars.toml` (optional `--link`). |
| `mars add <source>` | Add/update source, then sync. |
| `mars remove <source>` | Remove source, then prune managed items. |
| `mars sync` | Resolve + install to match config (`--force`, `--diff`, `--frozen`). |
| `mars upgrade [sources...]` | Maximize versions for all or selected sources. |
| `mars outdated` | Show available updates without applying changes. |
| `mars list` | List managed agents/skills (`--status` for checksum state). |
| `mars why <name>` | Explain source/provenance for an installed item. |
| `mars rename <from> <to>` | Add rename rule and sync to apply it. |
| `mars resolve [file]` | Mark conflicts resolved by updating installed checksums. |
| `mars override <source> --path <PATH>` | Set local dev override in `mars.local.toml`. |
| `mars link <target>` | Link managed `agents/` + `skills/` into tool dir (`--unlink`, `--force`). |
| `mars check [path]` | Validate a source package (structure/frontmatter/deps). |
| `mars doctor` | Validate config/lock/filesystem/link/dependency health. |
| `mars repair` | Force rebuild from config/sources (corrupt-lock recovery path). |

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success (or clean report). |
| `1` | Action needed: unresolved conflicts (sync) or validation errors (check). |
| `2` | User/config/resolution/validation/link/frozen errors. |
| `3` | Source fetch, I/O, HTTP, or git command errors. |

## Sync Pipeline

Sync is orchestrated in `src/sync/mod.rs` under `.mars/sync.lock`:

1. Load config + local overrides; apply optional mutation.
2. Resolve graph (versions, deps, source identities).
3. Build target state.
4. Compute diff.
5. Create plan.
6. Apply plan.
7. Write new `mars.lock`.

Core apply chain: `target -> diff -> plan -> apply`.

## Source Fetching

- GitHub HTTPS sources: archive download (`/archive/<sha>.tar.gz`) into global cache.
- SSH / non-GitHub HTTPS git sources: `git clone`/`git fetch` + checkout.
- Local path sources: canonicalized filesystem paths, read directly (no copy cache).

Global cache root: `~/.mars/cache` (override with `MARS_CACHE_DIR`).

## Key Modules

| Module | Responsibility |
|---|---|
| `src/cli/` | Clap args, root discovery, command dispatch, output formatting. |
| `src/config/` | `mars.toml` + `mars.local.toml` schemas, load/save, merge to effective config. |
| `src/lock/` | `mars.lock` schema, load/write, lock rebuild from apply outcomes. |
| `src/sync/` | End-to-end sync orchestration + `target/diff/plan/apply`. |
| `src/resolve/` | Dependency + version resolution and graph ordering. |
| `src/source/` | Source parsing/fetching (git + path) and global cache handling. |
| `src/discover/` | Discover agents/skills by filesystem conventions. |
| `src/validate/` | Agent-to-skill dependency validation. |
| `src/merge/` | Three-way merge/conflict handling. |
| `src/fs/` | Atomic writes/installs and advisory file lock (`flock`). |
| `src/frontmatter/` | YAML frontmatter parsing for agent/skill metadata. |
| `src/manifest/` | Optional per-source `mars.toml` manifest loading. |
| `src/hash/` | SHA-256 hashing for files/directories. |
| `src/types.rs` | Typed identifiers/newtypes used across modules. |

## Dev Workflow

```bash
cargo build
cargo test
cargo clippy --all-targets --all-features
cargo fmt --all
```

Notes:

- Integration coverage is under `tests/` (CLI-level behavior).
- Prefer keeping changes localized to one module/command path when adding features.
