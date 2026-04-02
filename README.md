# mars

Mars is an agent package manager for `.agents/` directories. It installs agent profiles and skills from multiple sources into one managed root, tracks ownership in `mars.lock`, and can link managed content into tool-specific folders like `.claude/`.

## Install

```bash
cargo install --git https://github.com/haowjy/mars-agents
```

## Quick Start (2 minutes)

```bash
# 1) Initialize managed root in current project
mars init

# 2) Add one or more sources
mars add haowjy/meridian-base
mars add ./my-local-agents

# 3) Install/update managed content
mars sync

# 4) Link into your tool directory (optional)
mars link .claude
```

## Common Commands

| Command | What it does |
|---|---|
| `mars init [TARGET]` | Create managed root with `mars.toml`. |
| `mars add <source>` | Add source and sync. |
| `mars remove <source>` | Remove source and prune managed items. |
| `mars sync` | Resolve + install to match config. |
| `mars upgrade [sources...]` | Upgrade sources to newer versions. |
| `mars outdated` | Show available updates without changing files. |
| `mars list` | Show managed agents/skills. |
| `mars why <name>` | Show why an item is installed. |
| `mars rename <from> <to>` | Rename a managed item via config rule. |
| `mars resolve [file]` | Mark conflict files as resolved. |
| `mars override <source> --path <path>` | Use a local path override for a source. |
| `mars link <target>` | Symlink `agents/` + `skills/` into tool dir. |
| `mars check [path]` | Validate a package before publishing. |
| `mars doctor` | Check config/lock/filesystem health. |
| `mars repair` | Rebuild state from config + sources. |

All commands support global flags: `--root <path>` and `--json`.

## `mars.toml` Example

```toml
[sources.base]
url = "https://github.com/haowjy/meridian-base"
version = "^1.0"

[sources.dev]
path = "../my-dev-agents"

[settings]
links = [".claude"]
```

After editing `mars.toml`, run:

```bash
mars sync
```
