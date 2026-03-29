# selective-testing

A high-performance Test Impact Analysis (TIA) tool for Swift/Xcode projects. Given a set of changed files, it identifies the minimal set of affected unit and snapshot tests — so you only run what matters.

## Problem

In large Swift codebases, running the full test suite on every commit wastes CI resources and delays developer feedback. A typical project may have thousands of tests taking 10–20 minutes, but most commits touch a handful of files — meaning 90%+ of tests are irrelevant to any given change.

## How It Works

selective-testing operates as a three-phase pipeline: **Index → Diff → Resolve**.

### Phase 1: Index

Builds a file-level dependency graph where nodes are `.swift` files and edges represent "is used by" relationships. The graph is constructed from one of two data sources:

- **IndexStoreDB (primary)** — Queries Xcode's index store via a Swift helper binary. Uses USR (Unified Symbol Resolution) identifiers for exact cross-module symbol matching. A type named `Constants` in module A is distinct from `Constants` in module B.
- **tree-sitter (fallback)** — Parses Swift source code directly using the tree-sitter grammar. No Xcode build required, but uses name-based matching which can produce false positives across modules.

The graph is serialized to a MessagePack binary cache (`.selective-testing/graph.bin`). Subsequent runs load the cache and incrementally update only changed files using mtime comparison.

### Phase 2: Diff

Uses libgit2 to compute the merge-base diff between your branch and the base branch (e.g., `origin/master`). This shows only the files **your branch** changed — not changes that happened on the base branch since you diverged. Uncommitted working directory changes are also detected.

### Phase 3: Resolve

Performs a depth-limited BFS traversal from each changed file, following outgoing edges to find affected test files. Each test kind has its own rules:

| Test Kind | Edges Followed | Depth Limit | Rationale |
|-----------|---------------|-------------|-----------|
| **Unit** | `DirectReference` | 2 | Tests use spies/mocks — transitive chains beyond direct callers are irrelevant |
| **Snapshot** | `DirectReference` + `ViewEmbedding` | 3 | Visual changes cascade through the view tree, but not beyond |

The traversal stops at test files (their dependencies are fakes, not real implementations) and at the depth limit (prevents fan-out through routers/coordinators that would select hundreds of unrelated tests).

Multiple test kinds are resolved in a single BFS pass.

## Performance

Benchmarked on a production iOS project with ~7,700 Swift files:

| Operation | Time |
|-----------|------|
| Full index (IndexStoreDB) | ~42s |
| Full index (tree-sitter fallback) | ~1s |
| Incremental index (1 file changed) | 0.15s |
| No changes detected | 0.07s |
| Resolve (cached graph) | 0.22s |

| Metric | Value |
|--------|-------|
| Files indexed | 7,737 |
| Edges (IndexStoreDB) | 38,792 |
| Edges (tree-sitter) | 206,700 |
| Graph cache size | 3.4 MB |

For a branch with 2 changed Swift files, selective-testing identified 6 unit tests and 1 snapshot test out of ~1,400 total test files.

## Installation

### From GitHub Releases

```bash
curl -sL https://github.com/gutiago/selective-testing/releases/download/1.0.0/selective-testing-darwin-arm64.tar.gz \
  | tar xz -C /usr/local/bin
```

This installs two binaries:
- `selective-testing` — the Rust CLI
- `index-helper` — the Swift IndexStoreDB bridge

### From Source

```bash
# Rust CLI
cargo build --release
# Binary at: target/release/selective-testing

# Swift helper (requires Xcode)
cd swift-helper && swift build --configuration release
# Binary at: swift-helper/.build/release/index-helper
```

## Usage

### Index the project

Build the dependency graph. Run this once, then it's cached.

```bash
# With IndexStoreDB (requires a prior Xcode build)
selective-testing index \
  --repo-root /path/to/project \
  --helper-path /usr/local/bin/index-helper

# With explicit index store path
selective-testing index \
  --repo-root /path/to/project \
  --helper-path /usr/local/bin/index-helper \
  --index-store ~/Library/Developer/Xcode/DerivedData/MyApp-abc123/Index.noindex/DataStore

# Force full re-index (ignore cache)
selective-testing index --repo-root /path/to/project --force
```

The index store is auto-detected from `~/Library/Developer/Xcode/DerivedData/` (picks the most recently modified match). If IndexStoreDB is unavailable, it falls back to tree-sitter automatically.

### Resolve affected tests

Find which tests need to run for your branch.

```bash
# All test kinds
selective-testing resolve --base origin/master

# Specific kinds
selective-testing resolve --base origin/master --kind unit
selective-testing resolve --base origin/master --kind snapshot
selective-testing resolve --base origin/master --kind unit --kind snapshot

# JSON output for CI integration
selective-testing resolve --base origin/master --format json

# xcodebuild flags (pipe directly)
selective-testing resolve --base origin/master --kind unit --format xcodebuild
# outputs: -only-testing:UnitTests/CartServiceTests -only-testing:UnitTests/PaymentTests
```

### Inspect the graph

```bash
# Graph summary
selective-testing graph --repo-root /path/to/project

# Dependencies for a specific file
selective-testing graph --file Sources/CartService.swift

# Detect dependency cycles
selective-testing graph --cycles
```

### Incremental updates

When you edit files locally, the graph updates incrementally on the next `index` run — only re-parses changed files using tree-sitter (no full IndexStoreDB re-query needed).

```bash
# Edit a file...
vim Sources/CartService.swift

# Incremental update: detects 1 changed file, re-indexes in ~0.15s
selective-testing index --repo-root /path/to/project
```

## CI Integration

### CircleCI

```yaml
commands:
  install-selective-testing:
    steps:
      - run:
          name: Install selective-testing
          command: |
            curl -sL https://github.com/gutiago/selective-testing/releases/download/1.0.0/selective-testing-darwin-arm64.tar.gz \
              | tar xz -C /usr/local/bin

jobs:
  unit-tests:
    macos:
      xcode: "16.3"
    steps:
      - checkout
      - install-selective-testing
      # Build first so index store is fresh
      - run: xcodebuild build -scheme MyApp
      # Index + resolve
      - run:
          name: Find affected tests
          command: |
            selective-testing index --repo-root . --helper-path /usr/local/bin/index-helper
            FLAGS=$(selective-testing resolve --base origin/master --kind unit --format xcodebuild)
            if [ -z "$FLAGS" ]; then
              echo "No affected tests, skipping."
              circleci-agent step halt
            else
              xcodebuild test -scheme MyApp $FLAGS
            fi
```

### GitHub Actions

```yaml
- name: Install selective-testing
  run: |
    curl -sL https://github.com/gutiago/selective-testing/releases/download/1.0.0/selective-testing-darwin-arm64.tar.gz \
      | tar xz -C /usr/local/bin

- name: Index
  run: selective-testing index --repo-root . --helper-path /usr/local/bin/index-helper

- name: Run affected unit tests
  run: |
    FLAGS=$(selective-testing resolve --base origin/main --kind unit --format xcodebuild)
    if [ -z "$FLAGS" ]; then
      echo "No affected tests."
    else
      xcodebuild test -scheme MyApp $FLAGS
    fi
```

### Caching the graph

Cache `.selective-testing/graph.bin` between CI runs to avoid full re-indexing:

```yaml
# CircleCI
- save_cache:
    key: selective-testing-graph-{{ checksum ".selective-testing/graph.bin" }}
    paths:
      - .selective-testing/

# GitHub Actions
- uses: actions/cache@v4
  with:
    path: .selective-testing/
    key: selective-testing-graph-${{ hashFiles('.selective-testing/graph.bin') }}
```

## Architecture

```
Swift Sources ──► git ls-files (file discovery)
                       │
                       ▼
IndexStoreDB  ──► Swift Helper ──► JSON edges ──┐
  (primary)       (subprocess)                   │
                                                 ├──► Dependency Graph ──► graph.bin
tree-sitter   ──► Parallel parse ──► edges ──────┘       (petgraph)        (MessagePack)
  (fallback)      (Rayon)                                    │
                                                             │
git diff      ──► Changed .swift files ──────────────► BFS Traversal
  (merge-base)                                         (depth-limited)
                                                             │
                                                             ▼
                                                       Affected Tests
                                                    (grouped by kind)
```

### File classification

Test files are classified automatically by path and import conventions:

| Pattern | Classification |
|---------|---------------|
| `Tests/*Tests.swift` | Unit test |
| `*SnapshotTests/*` or imports `SnapshotTesting` | Snapshot test |
| `*UITests/*` or `*E2ETests/*` | Source (excluded — UI tests need a different approach) |
| Everything else | Source |

### Data source priority

1. **IndexStoreDB** — symbol-level precision via USR. Requires a prior Xcode build. Auto-detected from `~/Library/Developer/Xcode/DerivedData/`.
2. **tree-sitter** — file-level analysis by parsing Swift syntax. No build required. Used as fallback or for incremental updates of individual files.
3. **Compiler .d files** — makefile-format dependency files from DerivedData. Fastest to parse but least precise. Available via `--derived-data` flag.

The principle: **over-select rather than under-select.** Running a few extra tests is far better than missing a broken dependency.

## Requirements

- macOS (the Swift helper links `libIndexStore.dylib` from Xcode)
- Xcode installed (for IndexStoreDB support)
- A prior Xcode build (to populate the index store)
- Git repository

For tree-sitter-only mode (no IndexStoreDB), only Git and the Rust binary are needed.
