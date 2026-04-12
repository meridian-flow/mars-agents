# mars

A package manager for AI agent profiles. Install agents and skills from git sources into Claude Code, Cursor, Codex, OpenCode — any tool that reads from a config directory.

## Install

| Method | Command |
|---|---|
| Cargo | `cargo install mars-agents` |
| pip / uv | `uv tool install mars-agents` or `pip install mars-agents` |
| npm | `npm install -g @meridian-flow/mars-agents` |
| Prebuilt binaries | [GitHub Releases](https://github.com/meridian-flow/mars-agents/releases) |

## Quick Start

```bash
mars init
mars add meridian-flow/meridian-dev-workflow
mars link .claude
mars link .cursor
```

Your agents and skills are now installed and available in both Claude Code and Cursor. Update them with `mars upgrade`, check for drift with `mars doctor`.

## Adding Sources

```bash
# From GitHub
mars add meridian-flow/meridian-base
mars add acme/security-agents --only-agents

# From a local directory
mars add ../my-team-agents

# Pin a version
mars add meridian-flow/meridian-base@^1.0
```

## Model Aliases

Packages can distribute model routing — short names that resolve to concrete models across harnesses:

```bash
mars models list
mars models resolve opus
```

```toml
# In mars.toml — override any alias
[models.opus]
harness = "claude"
provider = "Anthropic"
match = ["*opus*"]
```

## How It Works

```
mars.toml + mars.lock (committed)
    ↓ mars sync
  .mars/ (canonical store, gitignored)
    ↓ copy to each target
  .agents/, .claude/, .cursor/ (your tools read from here)
```

Mars resolves the full dependency graph before touching any files. Writes are atomic. The lock file tracks what mars manages so it never touches your files.

## Docs

- [Configuration](docs/configuration.md) — `mars.toml` reference, filters, model aliases, merge precedence
- [CLI Reference](docs/commands.md) — every command with flags and examples
- [Sync Pipeline](docs/sync-pipeline.md) — how resolve → plan → apply works
- [Conflicts](docs/conflicts.md) — collision handling and resolution
- [Local Development](docs/local-development.md) — overrides, local paths, submodules
- [Troubleshooting](docs/troubleshooting.md) — `mars doctor`, `mars repair`

## License

MIT
