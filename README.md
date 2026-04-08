# selective-testing

A high-performance Test Impact Analysis (TIA) tool for Swift/Xcode projects. Given a set of changed files, it identifies the minimal set of affected unit, snapshot, and UI tests — so you only run what matters.

## Problem

In large Swift codebases, running the full test suite on every commit wastes CI resources and delays developer feedback. A typical project may have thousands of tests taking 10–20 minutes, but most commits touch a handful of files — meaning 90%+ of tests are irrelevant to any given change.

## How It Works

selective-testing operates as a three-phase pipeline: **Index → Diff → Resolve**.

### Phase 1: Index

Builds a file-level dependency graph where nodes are `.swift` files and edges represent "is used by" relationships. The graph is constructed from one of two data sources:

- **IndexStoreDB (primary)** — Queries Xcode's index store via a Swift helper binary. Uses USR (Unified Symbol Resolution) identifiers for exact cross-module symbol matching. A type named `Constants` in module A is distinct from `Constants` in module B.
- **tree-sitter (fallback)** — Parses Swift source code directly using the tree-sitter grammar. No Xcode build required, but uses name-based matching which can produce false positives across modules. See [Name collision problem](#name-collision-problem-tree-sitter-fallback) below.

The graph is serialized to a MessagePack binary cache (`.selective-testing/graph.bin`). Subsequent runs load the cache and incrementally update only changed files using mtime comparison.

### Phase 2: Diff

Uses libgit2 to compute the merge-base diff between your branch and the base branch (e.g., `origin/master`). This shows only the files **your branch** changed — not changes that happened on the base branch since you diverged. Uncommitted working directory changes are also detected.

### Phase 3: Resolve

Performs a BFS traversal from each changed file, following outgoing edges to find affected test files. Each test kind has its own traversal rules because of how tests are written.

### Why different rules per test kind

Swift projects that follow dependency injection patterns typically have **tests that use test doubles (stubs, spies, and mocks)** to isolate the class under test from its real dependencies. Both unit tests and snapshot tests inject fakes — a test for `CartService` injects a `NetworkSpy` instead of the real `NetworkLayer`. This means:

- Changing `NetworkLayer.swift` should **not** trigger `CartServiceTests` — the test never touches `NetworkLayer`, it uses a spy.
- Changing `CartService.swift` **should** trigger `CartServiceTests` — the test directly exercises this class.

The difference between test kinds is how **views** and **accessibility identifiers** are handled. Snapshot tests render real SwiftUI views, so they must follow the view embedding hierarchy. UI tests interact with the app through accessibility identifiers, so they need a different bridging mechanism.

This leads to three distinct traversal strategies:

| Test Kind | Edges Followed | DirectReference Depth | ViewEmbedding Depth | Rationale |
|-----------|---------------|----------------------|--------------------|-----------|
| **Unit** | `DirectReference` | 1 hop | N/A | Only tests that directly reference the changed file |
| **Snapshot** | `DirectReference` + `ViewEmbedding` | 1 hop | Unlimited | Visual changes cascade through the entire view tree |
| **UI** | `DirectReference` + `ViewEmbedding` + `AccessibilityBinding` | 1 hop | Unlimited | Bridges source → test via shared accessibility identifiers |

**DirectReference depth limit (all kinds):** The traversal follows at most 1 `DirectReference` hop from the changed file — only tests that directly reference the changed file are selected. Indirect dependents (e.g., a ViewModel that uses the changed Service) are not followed, because their tests inject spies/stubs for the changed dependency and are unaffected.

**ViewEmbedding unlimited (snapshot + UI):** `ViewEmbedding` edges represent a view rendering another view inside its `body`. These hops don't count toward the depth limit. A change in a leaf view at depth 10 of the view hierarchy will correctly trigger the root screen's snapshot tests. Traversal follows both outgoing edges (up to embedders) and incoming edges (down to embedded child views), with a `going_down` flag to prevent sideways spreading to unrelated embedders.

**AccessibilityBinding (UI only):** `AccessibilityBinding` edges connect source files that set `.accessibilityIdentifier(...)` to test files that query the same identifier (via XCUIElement subscripts or custom page object helpers). These edges are built by matching accessibility identifier string literals across files. Crossing an `AccessibilityBinding` edge resets the DirectReference depth to 0, allowing the BFS to continue from the test's page objects to the test file itself.

The traversal always stops at test files — their dependencies are fakes, not real implementations.

Multiple test kinds are resolved in a single BFS pass.

### UI test method-level precision

For UI tests, the tool provides **method-level** granularity. After the BFS identifies affected UI test files, it post-filters to select only the specific test methods whose accessibility queries overlap with the impacted accessibility identifiers. This means a change to a login screen view only selects `testLogin`, not every test method in the file.

### Visual examples

These diagrams show how the BFS traces from a changed file to affected tests. Arrows represent edges in the graph — `A → B` means "A is used by B". Dashed arrows are blocked paths.

#### Unit test — DirectReference only, 1-hop depth limit

You change `CartService.swift`. The BFS follows `DirectReference` edges outward,
stopping at depth 1. Only tests that **directly reference** the changed file are
selected — indirect dependents inject spies/stubs and are unaffected.

Consider this dependency tree — each parent imports/uses the child:

```swift
class CartCoordinator {
    let vm: CartVMProtocol          // injected (spy in tests)
}
class CartVM {
    let service: CartServiceProtocol // injected (spy in tests)
}
```

The graph stores DirectReference edges from dependency → dependent ("is used by").
The BFS follows these edges outward from the changed file, but only 1 hop deep.
Each source node's unit test is shown on the right:

```
                  Dependency Tree                          Unit Tests
                  ───────────────                          ──────────

              ┌──────────────────┐           ┌──────────────────────────┐
              │ CartCoordinator  │╌╌╌╌╌╌╌╌╌► │ CartCoordinatorTests     │
              └──────────────────┘  BLOCKED  │ ✗ not selected           │
                       │           (hop 2)   │ (injects CartVMSpy)      │
                    uses                     └──────────────────────────┘
                       │
                       ▼
              ┌──────────────────┐           ┌──────────────────────────┐
              │ CartVM           │╌╌╌╌╌╌╌╌╌► │ CartVMTests              │
              └──────────────────┘  BLOCKED  │ ✗ not selected           │
                       │           (hop 2)   │ (injects CartServiceSpy) │
                    uses                     └──────────────────────────┘
                       │
                       ▼
              ╔══════════════════╗           ┌──────────────────────────┐
              ║ CartService      ║──────────►│ CartServiceTests         │
              ║ (changed)        ║ DirectRef │ ✓ selected (hop 1)       │
              ╚══════════════════╝  (hop 1)  │ (directly tests          │
                       │                     │  CartService)            │
                       │                     └──────────────────────────┘
                       │
                       │ DirectRef            ┌──────────────────────────┐
                       └─────────────────────►│ MediaCarouselItemBuilder │
                            (hop 1)           │ Tests                    │
                                              │ ✓ selected (hop 1)       │
                                              │ (directly references     │
                                              │  CartService to build    │
                                              │  media items)            │
                                              └──────────────────────────┘
```

- **`CartServiceTests`** (hop 1) — directly tests `CartService`. Selected.
- **`MediaCarouselItemBuilderTests`** (hop 1) — also directly references
  `CartService` (e.g., creates a real instance to build media carousel items).
  Since it uses the real class, not a spy, a change can break it. Selected.
- **`CartVMTests`** (hop 2) — blocked. `CartVMTests` injects a `CartServiceSpy`,
  never touching the real `CartService`. The change can't affect it.
- **`CartCoordinatorTests`** (hop 2) — also blocked for the same reason: injects a
  `CartVMSpy`, never touching the real `CartVM` or `CartService`.

#### Snapshot test — ViewEmbedding chain, unlimited depth

You change `AvatarView.swift`. Snapshot tests render real SwiftUI views, so a
visual change in a leaf view must propagate up to every screen that renders it.

Consider this view tree in Swift — each parent embeds child views in its `body`:

```swift
struct SettingsScreen: View {       // root
    var body: some View {
        ProfileHeader()             //   ├── ProfileHeader
        NotificationToggle()        //   └── NotificationToggle
    }
}
struct ProfileHeader: View {
    var body: some View {
        AvatarView()                //       └── AvatarView
    }
}
```

The graph stores ViewEmbedding edges from child → parent (the child "is rendered
by" the parent). The BFS follows these edges **with no depth limit**, propagating
up the tree. Every ancestor that renders the changed view is visually affected,
and each one's snapshot test is selected:

```
                        View Tree                         Snapshot Tests
                        ─────────                         ──────────────

                   ┌──────────────────┐          ┌──────────────────────────┐
                   │ SettingsScreen   │─────────►│ SettingsSnapshotTests    │
                   └──────────────────┘ DirectRef│ ✓ selected               │
                      │              │           └──────────────────────────┘
             ViewEmbedding    ViewEmbedding
                      │              │
                      ▼              ▼
          ┌────────────────┐  ┌────────────────┐  ┌────────────────────────┐
          │ ProfileHeader  │  │ Notification   │  │ NotificationSnapshot   │
          │                │  │ Toggle         │──►  Tests                 │
          └────────────────┘  └────────────────┘  │ (not affected)         │
                   │                              └────────────────────────┘
              ViewEmbedding
                   │
                   ▼
          ╔════════════════╗             ┌──────────────────────────┐
          ║ AvatarView     ║────────────►│ AvatarSnapshotTests      │
          ║ (changed)      ║  DirectRef  │ ✓ selected               │
          ╚════════════════╝             └──────────────────────────┘
```

Changing `AvatarView` triggers `AvatarSnapshotTests` (its own test) and
`SettingsSnapshotTests` (an ancestor that renders it). `NotificationSnapshotTests`
is **not** triggered — it's a sibling branch in the view tree, not an ancestor.

ViewEmbedding hops don't count toward the DirectReference depth limit, so the
final DirectRef hop to each snapshot test is still at depth 1 regardless of
how deep the view tree is.

#### UI test — AccessibilityBinding bridge + method-level precision

You change `PaymentService.swift`. Starting from one service file, the BFS
ripples through three edge types to reach UI tests across the view hierarchy:

```swift
struct CheckoutScreen: View {
    var body: some View {
        PaymentView()        // embeds PaymentView
        ShippingView()       // embeds ShippingView
    }
}
struct PaymentView: View {
    let service: PaymentService  // uses PaymentService directly
    var body: some View {
        Text(service.formattedAmount)
            .accessibilityIdentifier("pay_amount")
        Button("Pay")
            .accessibilityIdentifier("pay_button")
    }
}
```

The BFS traverses DirectReference → ViewEmbedding → AccessibilityBinding,
collecting impacted accessibility identifiers along the way. Each visited
view's a11y IDs become impacted, and any UI test method querying those IDs
is selected.

```
                 View Tree                        Page Objects            UI Tests
                 ─────────                        ────────────            ────────

            ┌───────────────────┐ 3 A11yBinding  ┌──────────────┐ DirectRef ┌─────────────────────┐
       ┌───►│ CheckoutScreen    │───────────────►│ CheckoutPage │──────────►│ CheckoutUITests     │
       │    │ "checkout_total"  │"checkout_total"│              │ (hop 1)   │ testCheckout()  ✓   │
       │    └───────────────────┘  (depth → 0)   └──────────────┘           │ testPromoCode() ✗   │
       │        │            │                                              └─────────────────────┘
  2 ViewEmbed   │            │
    (↑ up)  4 ViewEmbed  ViewEmbed
       │      (↓ down)   (↓ down)
       │        │            │
       │        ▼            ▼
       │   ┌──────────────┐  ┌──────────────┐
       │   │ ShippingView │  │ PromoCodeView│
       │   │ "ship_addr"  │  │ (no a11y IDs │
       │   └──────────────┘  │  — no bridge)│
       │        │            └──────────────┘
       │        │
       │   3 A11yBinding    ┌──────────────┐  DirectRef   ┌─────────────────────┐
       │    "ship_addr" ───►│ ShippingPage │─────────────►│ ShippingUITests     │
       │    (depth → 0)     │              │   (hop 1)    │ testAddress()  ✓    │
       │                    └──────────────┘              │ testTracking() ✗    │
       │                                                  └─────────────────────┘
       │
  ┌───────────────────┐      3 A11yBinding  ┌──────────────┐ DirectRef ┌─────────────────────┐
  │ PaymentView       │────────────────────►│ PaymentPage  │──────────►│ PaymentUITests      │
  │ "pay_button"      │     "pay_button"    │              │ (hop 1)   │ testPay()      ✓    │
  │ "pay_amount"      │     (depth → 0)     └──────────────┘           │ testAmount()   ✓    │
  └───────────────────┘                                                └─────────────────────┘
           ▲
           │
  1 DirectRef (hop 1)
           │
  ╔═══════════════════╗
  ║ PaymentService    ║
  ║ (changed)         ║
  ╚═══════════════════╝
```

**How the BFS reaches 3 UI test files from 1 changed service:**

1. **DirectRef** — `PaymentService` → `PaymentView` (hop 1). The view directly
   uses the service. Its a11y identifiers `"pay_button"` and `"pay_amount"`
   are now impacted.

2. **ViewEmbedding ↑** — `PaymentView` → `CheckoutScreen` (up to embedder).
   ViewEmbedding hops don't count toward the depth limit. `CheckoutScreen`'s
   a11y identifier `"checkout_total"` is added to the impacted set.

3. **AccessibilityBinding** — each visited view with impacted a11y IDs bridges
   to page objects that query the same identifiers. Crossing this edge
   **resets depth to 0**, then a DirectRef hop (1) reaches the test file.

4. **ViewEmbedding ↓** — `CheckoutScreen` → `ShippingView`, `PromoCodeView`
   (down into embedded children). Enters `going_down` mode to prevent
   spreading back up to unrelated parent screens. `ShippingView`'s
   `"ship_addr"` is added to the impacted set. `PromoCodeView` has no a11y
   identifiers — it's visited but creates no bridge.

5. **Method filtering** — after the BFS, each UI test is filtered to only the
   methods that query impacted identifiers:
   - `testPay()` queries `"pay_button"` → **selected**
   - `testAmount()` queries `"pay_amount"` → **selected**
   - Both methods exercise `PaymentView`, which is affected — so all of
     `PaymentUITests` runs
   - `testCheckout()` queries `"checkout_total"` → **selected**
   - `testPromoCode()` queries `"promo_input"` → not impacted (set by an
     unvisited view outside this tree) → **skipped**
   - `testAddress()` queries `"ship_addr"` → **selected**
   - `testTracking()` queries `"tracking_number"` → not impacted (set by
     `TrackingView`, not in the affected view tree) → **skipped**

#### Combined: all three kinds in one graph

In practice, a single changed file can affect all three test kinds simultaneously.
The tool resolves them in a **single BFS pass** using the widest edge set.

```
              ╔═════════════════════╗           ┌──────────────────────┐
              ║ ProfileView.swift   ║──────────►│ ProfileViewTests     │ (unit)     ✓
              ║ (changed)           ║ DirectRef └──────────────────────┘
              ║ sets "profile_name" ║   (hop 1)
              ╚═════════════════════╝
                    │            │
             ViewEmbedding  A11yBinding
              (depth 0)    (resets to 0)
                    │            │
                    ▼            ▼
           ┌───────────────┐ ┌────────────────┐  DirectRef  ┌──────────────────┐
           │ SettingsScreen│ │ ProfilePage    │────────────►│ ProfileUITests   │
           └───────────────┘ │ "profile_name" │   (hop 1)   │                  │
                    │        └────────────────┘             │ testProfile()  ✓ │
               DirectRef                                    │ testSettings() ✗ │
                (hop 1)                                     └──────────────────┘
                    ▼                                        (ui, method
           ┌──────────────────────┐                          precision)
           │ SettingsSnapshot     │ (snapshot)  ✓
           │ Tests                │
           └──────────────────────┘
```

One change, three kinds of tests found — each through different edge types, each with
appropriate precision for how that kind of test actually exercises the code.

### Customizing traversal rules

The default traversal rules reflect the patterns used in my own projects — dependency injection with test doubles and SwiftUI view hierarchies. Your codebase may follow different conventions. Here's how to adjust.

The traversal logic is in [`src/graph/traversal.rs`](src/graph/traversal.rs):

**Change the DirectReference depth limit** — modify the `depth < 1` check in the edge expansion:

```rust
EdgeKind::DirectReference => {
    if direct_ref_depth < 1 {  // ← change this value
        queue.push_back((edge.target(), direct_ref_depth + 1, going_down));
    }
}
```

Increasing to 2 means tests of indirect dependents (e.g., a ViewModel that uses the changed Service) will also be selected — useful if your tests don't inject spies for all dependencies.

**Add a new test kind** — add a variant to `TestKind` in [`src/graph/model.rs`](src/graph/model.rs) and define which edges it follows:

```rust
pub enum TestKind {
    Unit,
    Snapshot,
    UITest,
    // YourNewKind,
}

impl TestKind {
    pub fn allowed_edges(&self) -> &[EdgeKind] {
        match self {
            TestKind::Unit => &[EdgeKind::DirectReference],
            TestKind::Snapshot => &[EdgeKind::DirectReference, EdgeKind::ViewEmbedding],
            TestKind::UITest => &[EdgeKind::DirectReference, EdgeKind::ViewEmbedding, EdgeKind::AccessibilityBinding],
            // TestKind::YourNewKind => &[...],
        }
    }
}
```

**Change file classification** — modify the path/import heuristics in [`src/swift/file_classifier.rs`](src/swift/file_classifier.rs) to match your project's conventions.

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

### Homebrew

```bash
brew tap gutiago/tap
brew install selective-testing
```

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
selective-testing resolve --base origin/master --kind ui
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
| `*UITests/*` | UI test |
| `*E2ETests/*` | E2E test |
| Everything else | Source |

### Data source priority

1. **IndexStoreDB** — symbol-level precision via USR. Requires a prior Xcode build. Auto-detected from `~/Library/Developer/Xcode/DerivedData/`.
2. **tree-sitter** — file-level analysis by parsing Swift syntax. No build required. Used as fallback or for incremental updates of individual files.
3. **Compiler .d files** — makefile-format dependency files from DerivedData. Fastest to parse but least precise. Available via `--derived-data` flag.

The principle: **over-select rather than under-select.** Running a few extra tests is far better than missing a broken dependency.

## Name Collision Problem (tree-sitter fallback)

When IndexStoreDB is unavailable, the tool falls back to tree-sitter, which matches types by **name only** — without module scope. This causes over-selection in large modular projects.

The most common offender is `Constants` — a nested enum that many Swift files define locally. In a ~7,700 file project, **355 files** define their own `enum Constants` and **522 files** reference `Constants.something`. tree-sitter can't distinguish between them, so a change to `Avatar.swift` (which defines `Constants` internally) creates false edges to hundreds of unrelated files.

To mitigate this, tree-sitter prefixes nested types with their parent type name. A `Constants` enum inside `struct Avatar` is stored as `Avatar.Constants`, which won't match a bare `Constants` reference in another module. This reduces false edges significantly but doesn't eliminate them entirely — top-level types with common names (like `Avatar` itself) can still collide.

IndexStoreDB avoids this entirely because each symbol has a USR (Unified Symbol Resolution) that encodes the full module path — `s:19DesignSystemSwiftUI6AvatarV9ConstantsO` is distinct from `s:8Training21TrainingInteractorImplC9ConstantsO`.

**Real-world impact on a ~7,700 file project (3 files changed):**

| Data Source | Unit Tests Selected | Edges in Graph |
|-------------|-------------------|----------------|
| IndexStoreDB | 4 | 38,656 |
| tree-sitter (with nested prefixing) | 23 | 163,615 |
| tree-sitter (without prefixing) | 61 | 215,471 |

**Recommendation:** Always use IndexStoreDB in CI. The build step runs before testing, so the index store is always fresh. tree-sitter is a fallback for local development where no Xcode build has been done yet — expect some over-selection.

## Caching

The dependency graph is cached at `.selective-testing/` in the repo root. Cache files are named by data source:

- `graph-indexstore.bin` — built from IndexStoreDB
- `graph-tree-sitter.bin` — built from tree-sitter fallback

When loading, the tool prefers `indexstore` over `tree-sitter`. This prevents accidentally using a less precise tree-sitter cache when an IndexStoreDB cache is available.

Incremental updates compare file mtimes against the cached graph and only re-parse changed files using tree-sitter (no full IndexStoreDB re-query needed).

## Requirements

- macOS (the Swift helper links `libIndexStore.dylib` from Xcode)
- Xcode installed (for IndexStoreDB support)
- A prior Xcode build (to populate the index store)
- Git repository

For tree-sitter-only mode (no IndexStoreDB), only Git and the Rust binary are needed.

## License

MIT License. See [LICENSE](LICENSE) for details.
