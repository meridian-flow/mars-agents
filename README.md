# mars

Mars is a package manager for agent directories. It installs agent profiles and skills from git and local sources into a `.mars/` canonical store, records ownership in `mars.lock`, and copies managed content into configured target directories (`.agents/`, `.claude/`, `.cursor/`, etc.).

## Quick Start

```bash
# Initialize with a target directory
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

- Install agents and skills from multiple sources into one managed tree
- Resolve versions and transitive source dependencies before installation
- Keep syncs safe: resolve full desired state, then apply atomically
- Track ownership and checksums in `mars.lock` so managed and unmanaged files coexist
- Copy managed content to multiple target directories (`.agents/`, `.claude/`, etc.)
- Support day-to-day maintenance: upgrades, outdated checks, local overrides, rename rules, conflict resolution, repair flows
- Package-distributed model aliases — no hardcoded builtins in the binary

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
| Targets | `init [--link]`, managed target configuration via `settings.targets` |
| Model aliases | `models list`, `models refresh`, `models resolve` |
| Validation & recovery | `check`, `doctor`, `repair` |
| Cache | `cache info`, `cache clean` |

Global flags: `--root <PATH>`, `--json`.

## How It Works

```
mars.toml + mars.lock (committed)
        ↓ mars sync
    .mars/ (canonical store, gitignored)
        ↓ copy to each target
    .agents/, .claude/, .cursor/ (committed, may contain non-mars content)
```

Every mutating command runs a typed pipeline:

```
load_config → resolve_graph → build_target → create_plan → apply_plan → sync_targets → finalize
```

1. **Resolve** — fetch sources, discover transitive deps, merge model aliases from dependency tree
2. **Build target** — discover items, apply filters, detect collisions
3. **Plan** — diff desired state against lock + disk
4. **Apply** — write resolved content to `.mars/` (atomic writes via tmp+rename)
5. **Sync targets** — copy from `.mars/` to each configured target directory (never deletes files mars didn't create)
6. **Finalize** — write lock, persist dependency model aliases, build report

## Managed Layout

```
project/
  mars.toml          # Dependency config (committed)
  mars.lock          # Ownership registry (committed)
  mars.local.toml    # Dev overrides (gitignored)
  .mars/             # Canonical store (gitignored)
    agents/          # Resolved agent profiles
    skills/          # Resolved skills
    models-cache.json      # Cached model catalog
    models-merged.json     # Dependency-sourced model aliases
  .agents/           # Target directory (committed, may have non-mars content)
    agents/
    skills/
  .claude/           # Another target (committed)
    agents/
    skills/
```

## Model Aliases

Model aliases are package-distributed — no builtins in the mars binary. Packages define aliases in their `mars.toml` under `[models]`:

```toml
# Pinned — explicit model ID
[models.opus]
harness = "claude"
model = "claude-opus-4-6"

# Auto-resolve — pattern matching against cached model catalog
[models.sonnet]
harness = "claude"
provider = "Anthropic"
match = ["sonnet"]
exclude = ["thinking"]
```

Merge precedence: consumer config > dependencies (declaration order, first wins).

```bash
mars models refresh          # Fetch model catalog from API
mars models list             # Show all aliases (deps + consumer config)
mars models list --include "opus*,sonnet*"   # Show only matching aliases
mars models list --exclude "experimental-*"   # Hide matching aliases
mars models resolve opus     # Resolve an alias to a concrete model ID
```

`--include` and `--exclude` are mutually exclusive. Both override `[settings.model_visibility]` for that command run.

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

[models.opus]
harness = "claude"
provider = "Anthropic"
match = ["opus"]

[settings]
targets = [".agents", ".claude"]
```

After editing `mars.toml`, run `mars sync` to apply changes.

## Documentation

Detailed documentation is in [`docs/`](docs/):

- **[Overview](docs/README.md)** — Core concepts and quick start
- **[Configuration](docs/configuration.md)** — `mars.toml` reference: all fields, filter modes, settings
- **[CLI Reference](docs/commands.md)** — Every subcommand with flags, examples, and behavior
- **[Sync Pipeline](docs/sync-pipeline.md)** — How sync works: resolve → target → diff → apply → sync targets → finalize
- **[Conflicts](docs/conflicts.md)** — Collision handling, merge, conflict resolution
- **[Lock File](docs/lock-file.md)** — Lock file format and semantics
- **[Local Development](docs/local-development.md)** — Overrides, local paths, submodules
- **[Troubleshooting](docs/troubleshooting.md)** — `mars doctor`, `mars repair`, common problems

## Design Constraints

- Resolve first, then act. If resolution fails, nothing is mutated.
- Config, lock, and installed files use atomic writes (tmp+rename).
- `mars.lock` is the authority for what Mars manages.
- Target directories are shared — mars never deletes files it didn't create.
- User intent comes from explicit flags and arguments, not heuristics.
- No builtin model aliases — all aliases come from packages or consumer config.
