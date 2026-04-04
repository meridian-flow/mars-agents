# mars

Mars is a package manager for `.agents/` directories. It installs agent profiles and skills from git and local sources into one managed root, records managed ownership in `mars.lock`, and links that managed content into tool-specific directories like `.claude/` or `.cursor/`.

## Quick Start

```bash
# Initialize and link to Claude
mars init --link .claude

# Add sources
mars add haowjy/meridian-base
mars add haowjy/meridian-dev-workflow

# See what's installed
mars list

# Explain why an item is present
mars why reviewer
```

## Why Mars

- Install agents and skills from multiple sources into one managed `.agents/` tree
- Resolve versions and transitive source dependencies before installation
- Keep syncs safe: resolve the full desired state before mutating files
- Track ownership and checksums in `mars.lock` so managed and unmanaged files coexist safely
- Support day-to-day maintenance with upgrades, outdated checks, local overrides, rename rules, conflict resolution, and repair flows
- Link managed `agents/` and `skills/` into tool directories instead of copying them around

## Where Mars Fits

Mars is not an agent runtime. It sits underneath tools like Claude Code, Cursor, and Codex and manages the agent assets they read.

| Alternative | What Mars adds |
|---|---|
| Native `.claude/agents` | Multi-source installs, versioning, lockfile-backed ownership, safer syncs, recovery |
| Skill installers | Both agents and skills, explicit desired state in config, conflict handling, repair |
| Git submodules / vendored folders | Real resolution, diff, plan, and apply pipeline |

## Install

| Method | Command |
|---|---|
| Cargo (crate) | `cargo install mars-agents` |
| Cargo (latest main) | `cargo install --git https://github.com/haowjy/mars-agents` |
| Python (pipx) | `pipx install mars-agents` |
| Python (uv tool) | `uv tool install mars-agents` |
| Python (pip) | `pip install mars-agents` |
| npm | `npm install -g @haowjy/mars-agents` |
| From source | `cargo install --path .` |

Prebuilt binaries: <https://github.com/haowjy/mars-agents/releases>

Platforms: macOS arm64/x64, Linux arm64/x64 (glibc). Others: build from source.

## Source Inputs

| Form | Example |
|---|---|
| GitHub shorthand | `owner/repo` or `owner/repo@^1.0` |
| HTTPS URL | `https://github.com/owner/repo` |
| SSH URL | `git@github.com:owner/repo.git` |
| Local path | `../my-agents` or `/absolute/path` |

## Commands

| Area | Commands |
|---|---|
| Source management | `add`, `remove`, `upgrade`, `outdated`, `override` |
| Resolution | Semver constraints, transitive deps, lockfile-backed replay |
| Install & reconcile | `sync`, `rename`, `resolve` |
| Inspection | `list`, `why` |
| Linking | `init --link`, `link` |
| Validation & recovery | `check`, `doctor`, `repair` |
| Cache | `cache info`, `cache clean` |

Global flags: `--root <PATH>`, `--json`.

## Managed Layout

```
project/
  mars.toml          # Dependency config (committed)
  mars.lock          # Ownership registry (committed)
  mars.local.toml    # Dev overrides (gitignored)
  .mars/             # Internal state (gitignored)
  .agents/
    agents/
    skills/
  .claude/
    agents -> ../.agents/agents
    skills -> ../.agents/skills
```

## `mars.toml` Example

```toml
[dependencies.base]
url = "https://github.com/haowjy/meridian-base"
version = "^1.0"

[dependencies.dev]
path = "../my-dev-agents"

[dependencies.ops]
url = "https://github.com/acme/ops-agents"
only_skills = true

[settings]
links = [".claude"]
```

After editing `mars.toml`, run `mars sync` to apply changes.

## Documentation

Detailed documentation is in [`docs/`](docs/):

- **[Overview](docs/README.md)** — Core concepts and quick start
- **[Configuration](docs/configuration.md)** — `mars.toml` reference: all fields, filter modes, settings
- **[CLI Reference](docs/commands.md)** — Every subcommand with flags, examples, and behavior
- **[Sync Pipeline](docs/sync-pipeline.md)** — How sync works: resolve → target → diff → apply
- **[Conflicts](docs/conflicts.md)** — Collision handling, merge, conflict resolution
- **[Lock File](docs/lock-file.md)** — Lock file format and semantics
- **[Local Development](docs/local-development.md)** — Overrides, local paths, submodules
- **[Troubleshooting](docs/troubleshooting.md)** — `mars doctor`, `mars repair`, common problems

## Design Constraints

- Resolve first, then act. If resolution fails, nothing is mutated.
- Config, lock, and installed files use atomic writes.
- `mars.lock` is the authority for what Mars manages.
- User intent comes from explicit flags and arguments, not heuristics.
