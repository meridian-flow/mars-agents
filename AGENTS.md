# mars-agents

Agent package manager for `.agents/`. Manages agent profiles and skills across tools (Claude Code, Cursor, Codex, Gemini CLI).

## Core Principles

1. **Resolve first, then act.** Every command that mutates the filesystem fully resolves the target state first (version resolution, dependency checking, conflict detection, diff computation), then applies changes. If any conflict or error is detected during resolution, zero mutations occur. The user sees all problems at once, fixes them, and retries. No partial state. This applies to `sync`, `link`, `add`, `remove` — every write path.
   - *Why*: Partial failures leave users in states that are hard to diagnose and harder to recover from. A user who sees "3 conflicts" can fix all three and retry. A user whose command half-completed has to figure out what happened before they can fix anything. The resolve-first model also means the error output is complete — you never get "fixed conflict 1, now here's conflict 2" one at a time.

2. **Managed root is always a subdirectory.** The directory containing `mars.toml` (e.g. `.agents/`, `.claude/`) is always a child of the project root. `root.parent()` is always the project root. This invariant is enforced at construction (`MarsContext::new`) and assumed everywhere.
   - *Why*: Local source paths in config (`path = "./my-agents"`) resolve relative to the project root. Symlink targets in `mars link` resolve relative to the project root. If the managed root IS the project root, `root.parent()` goes one level too high and everything resolves wrong. A single invariant checked once at construction prevents an entire class of path resolution bugs.

3. **Atomic writes.** Every file write uses tmp + rename. Crash mid-write leaves the old file intact. No torn state on disk. Recovery IS startup — if mars is killed, the next command sees consistent state.
   - *Why*: Mars manages files that agents and tools read continuously. A half-written `mars.toml` or truncated agent profile breaks every tool that reads `.agents/`. tmp + rename is atomic on POSIX — the file is either the old version or the new version, never a partial write. No recovery logic needed because there's nothing to recover from.

4. **Lock file is authority.** `mars.lock` is the single source of truth for what's installed. If it's not in the lock, it's not managed. If the lock says it's there but disk disagrees, that's drift (doctor catches it, repair fixes it).
   - *Why*: Without a single authority, mars can't distinguish "user added this file manually" from "mars installed this and something changed it." The lock makes ownership explicit — mars only touches files it owns, and ownership is determined by lock presence, not filename patterns or directory location. This is what makes it safe to have unmanaged files coexist with managed ones.

5. **No heuristics.** User intent is expressed through explicit flags and arguments, not inferred from string patterns. `mars init .claude` works because `.claude` is a directory name, not because it starts with `.`. If ambiguity exists, error with guidance rather than guess.
   - *Why*: Heuristics are write-once-debug-forever. The dot-prefix heuristic (`starts_with('.')`) classified `./my-project` as a target dir — a real bug caught by reviewers. Every heuristic is a future bug report from a user whose input didn't match the assumed pattern. Explicit arguments are boring but predictable, and predictable tools earn trust.

## Architecture

```
project/
  .agents/                 <- managed root (default)
    mars.toml            <- config: sources, settings, links
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

Default: latest semver tag. Fallback: default branch HEAD. Constraints in `mars.toml` use semver ranges (`^1.0`, `~2.3`, `>=1.0 <3.0`). Pin to exact ref with `ref:` prefix.
