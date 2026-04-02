# mars-agents

Agent package manager for `.agents/`. Manages agent profiles and skills across tools (Claude Code, Cursor, Codex, Gemini CLI).

## Core Principles

1. **Resolve first, then act.** Every command that mutates the filesystem fully resolves the target state first (version resolution, dependency checking, conflict detection, diff computation), then applies changes. If any conflict or error is detected during resolution, zero mutations occur. The user sees all problems at once, fixes them, and retries. No partial state. This applies to `sync`, `link`, `add`, `remove` — every write path.

2. **Managed root is always a subdirectory.** The directory containing `agents.toml` (e.g. `.agents/`, `.claude/`) is always a child of the project root. `root.parent()` is always the project root. This invariant is enforced at construction (`MarsContext::new`) and assumed everywhere.

3. **Atomic writes.** Every file write uses tmp + rename. Crash mid-write leaves the old file intact. No torn state on disk. Recovery IS startup — if mars is killed, the next command sees consistent state.

4. **Lock file is authority.** `mars.lock` is the single source of truth for what's installed. If it's not in the lock, it's not managed. If the lock says it's there but disk disagrees, that's drift (doctor catches it, repair fixes it).

5. **Config mutations under lock.** All changes to `agents.toml` go through `ConfigMutation` variants executed under `.mars/sync.lock`. No direct load-modify-save outside the lock. This prevents lost updates from concurrent operations.

6. **No heuristics.** User intent is expressed through explicit flags and arguments, not inferred from string patterns. `mars init .claude` works because `.claude` is a directory name, not because it starts with `.`. If ambiguity exists, error with guidance rather than guess.

## Architecture

```
project/
  .agents/                 <- managed root (default)
    agents.toml            <- config: sources, settings, links
    .mars/                 <- internal (sync.lock, cache)
    agents/                <- installed agent profiles (.md)
    skills/                <- installed skills (dirs with SKILL.md)
  .claude/                 <- linked tool dir (optional)
    agents -> ../.agents/agents
    skills -> ../.agents/skills
```

## Dev Workflow

```bash
cargo build              # build
cargo test               # unit + integration tests (321 tests)
cargo clippy             # lint
```

Commit after each step that passes tests. Don't accumulate changes.

### Testing

Integration tests in `tests/integration/mod.rs` use temp dirs with local path sources. They test the full CLI binary end-to-end. Unit tests are co-located in each module.

### Key Modules

- `src/cli/` — Command dispatch, argument parsing, output formatting
- `src/sync/` — The sync pipeline: config mutation → resolve → diff → apply
- `src/resolve/` — Version resolution, dependency graph construction
- `src/source/` — Source fetching: git archives, system git clone, local paths
- `src/lock/` — Lock file read/write, integrity tracking
- `src/config/` — Config loading, merging, migration
- `src/validate/` — Agent → skill dependency checking
- `src/frontmatter/` — YAML frontmatter parsing for agent/skill markdown files

### Source Fetching

Three source types:
- **GitHub HTTPS** → archive download (`/archive/{sha}.tar.gz`), extracted with `flate2`/`tar`
- **SSH / non-GitHub HTTPS** → system `git clone --depth 1`
- **Local path** → direct filesystem read (no copy)

Global cache at `~/.mars/cache/` (override with `MARS_CACHE_DIR`).

### Version Resolution

Default: latest semver tag. Fallback: default branch HEAD. Constraints in `agents.toml` use semver ranges (`^1.0`, `~2.3`, `>=1.0 <3.0`). Pin to exact ref with `ref:` prefix.
