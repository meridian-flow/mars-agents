#!/usr/bin/env bash

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  cat <<'EOF'
Usage:
  scripts/release.sh [patch|minor|major|X.Y.Z] [--push] [--remote origin]

Behavior:
  - Runs pre-release checks (fmt, clippy, tests, release build)
  - Validates version consistency across all version files
  - Updates version in Cargo.toml, pyproject.toml, and npm packages
  - Creates a release commit
  - Creates an annotated git tag named v<version>
  - Optionally pushes the branch and tag

Examples:
  scripts/release.sh patch           # dry run (no push)
  scripts/release.sh patch --push    # release and push
  scripts/release.sh 1.2.3 --push    # explicit version
EOF
}

die() {
  printf '%s\n' "$*" >&2
  exit 1
}

warn() {
  printf 'WARNING: %s\n' "$*" >&2
}

FAILURES=()

check() {
  local name="$1"
  local cmd="$2"
  printf '%s... ' "$name"
  if eval "$cmd" >/dev/null 2>&1; then
    printf 'pass\n'
    return 0
  else
    printf 'FAIL\n'
    FAILURES+=("check failed: $name")
    return 1
  fi
}

require_clean_tree() {
  if [[ -n "$(git -C "$ROOT_DIR" status --short)" ]]; then
    FAILURES+=("working tree is not clean; commit or stash changes first")
    return 1
  fi
  return 0
}

require_branch() {
  local branch
  branch="$(git -C "$ROOT_DIR" branch --show-current)"
  if [[ -z "$branch" ]]; then
    FAILURES+=("release script must run from a branch, not detached HEAD")
    return 1
  fi
  printf '%s\n' "$branch"
  return 0
}

read_cargo_version() {
  grep '^version' "$ROOT_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/'
}

read_pypi_version() {
  grep '^version' "$ROOT_DIR/pyproject.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/'
}

read_npm_versions() {
  local pkg_dir="$ROOT_DIR/npm/@meridian-flow"
  local versions=()
  
  for pkg in "$pkg_dir"/mars-agents*/package.json; do
    if [[ -f "$pkg" ]]; then
      local v
      v="$(node -e "console.log(require('$pkg').version)")"
      versions+=("$v")
    fi
  done
  
  printf '%s\n' "${versions[@]}"
}

validate_version() {
  local version="$1"
  [[ "$version" =~ ^[0-9]+(\.[0-9]+){2}$ ]]
}

next_version() {
  local bump="$1"
  local current="$2"
  IFS='.' read -r major minor patch <<<"$current"

  case "$bump" in
    patch) patch=$((patch + 1)) ;;
    minor) minor=$((minor + 1)); patch=0 ;;
    major) major=$((major + 1)); minor=0; patch=0 ;;
    *) die "unknown bump kind: $bump" ;;
  esac

  printf '%s\n' "$major.$minor.$patch"
}

write_version() {
  local version="$1"
  
  sed -i "s/^version = \".*\"/version = \"$version\"/" "$ROOT_DIR/Cargo.toml"
  sed -i "s/^version = \".*\"/version = \"$version\"/" "$ROOT_DIR/pyproject.toml"
  
  local pkg_dir="$ROOT_DIR/npm/@meridian-flow"
  for pkg in "$pkg_dir"/mars-agents*/package.json; do
    if [[ -f "$pkg" ]]; then
      node -e "
        const pkg = require('$pkg');
        pkg.version = '$version';
        if (pkg.optionalDependencies) {
          for (const dep of Object.keys(pkg.optionalDependencies)) {
            pkg.optionalDependencies[dep] = '$version';
          }
        }
        require('fs').writeFileSync('$pkg', JSON.stringify(pkg, null, 2) + '\n');
      "
    fi
  done
  
  (cd "$ROOT_DIR" && cargo check --quiet 2>/dev/null)
}

main() {
  [[ $# -ge 1 ]] || {
    usage
    exit 1
  }

  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
  esac

  local target="$1"
  shift

  local push_remote=""
  local remote="origin"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --push)
        push_remote="1"
        shift
        ;;
      --remote)
        [[ $# -ge 2 ]] || die "--remote requires a value"
        remote="$2"
        shift 2
        ;;
      *)
        die "unknown argument: $1"
        ;;
    esac
  done

  printf 'Running pre-release checks...\n\n'

  local branch
  require_branch && branch=$(require_branch)
  require_clean_tree

  check "cargo fmt" "cd $ROOT_DIR && cargo fmt --check"
  check "cargo clippy" "cd $ROOT_DIR && cargo clippy --all-targets -- -D warnings"
  check "cargo test" "cd $ROOT_DIR && cargo test"
  check "cargo build --release" "cd $ROOT_DIR && cargo build --release"

  local cargo_version
  cargo_version="$(read_cargo_version)"
  local pypi_version
  pypi_version="$(read_pypi_version)"
  
  check "version: cargo == pypi" "[[ '$cargo_version' = '$pypi_version' ]]"

  if [[ ${#FAILURES[@]} -gt 0 ]]; then
    printf '\n=== PRE-RELEASE CHECKS FAILED ===\n\n'
    for f in "${FAILURES[@]}"; do
      printf '  - %s\n' "$f"
    done
    printf '\nFix the issues above and try again.\n'
    exit 1
  fi

  printf '\nAll pre-release checks passed.\n\n'

  local next_version_value
  case "$target" in
    patch|minor|major)
      next_version_value="$(next_version "$target" "$cargo_version")"
      ;;
    *)
      next_version_value="$target"
      ;;
  esac

  validate_version "$next_version_value" || die "invalid version: $next_version_value"
  [[ "$next_version_value" != "$cargo_version" ]] || die "next version matches current: $cargo_version"

  local tag="v$next_version_value"
  if git -C "$ROOT_DIR" rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    die "tag already exists: $tag"
  fi

  printf 'Bumping version: %s -> %s\n' "$cargo_version" "$next_version_value"
  write_version "$next_version_value"

  local version_files=("Cargo.toml" "pyproject.toml" "Cargo.lock")
  for pkg in "$ROOT_DIR/npm/@meridian-flow"/mars-agents*/package.json; do
    if [[ -f "$pkg" ]]; then
      version_files+=("${pkg#$ROOT_DIR/}")
    fi
  done
  
  git -C "$ROOT_DIR" add "${version_files[@]}"
  git -C "$ROOT_DIR" commit -m "release: v$next_version_value"
  git -C "$ROOT_DIR" tag -a "$tag" -m "Release $next_version_value"

  printf '\nReleased %s on branch %s\n' "$next_version_value" "$branch"
  printf 'Created commit and tag %s\n' "$tag"

  if [[ -n "$push_remote" ]]; then
    git -C "$ROOT_DIR" push "$remote" "$branch"
    git -C "$ROOT_DIR" push "$remote" "$tag"
    printf 'Pushed branch %s and tag %s to %s\n' "$branch" "$tag" "$remote"
  else
    printf '\nNothing pushed. Run:\n'
    printf '  git push %s %s\n' "$remote" "$branch"
    printf '  git push %s %s\n' "$remote" "$tag"
  fi
}

main "$@"