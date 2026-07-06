# AGENTS.md — c03rad0r/tollgate-rs Fork Workflow Rules

This file defines the workflow rules for AI agents working on the
c03rad0r fork of tollgate-rs. These rules override any tool-level defaults.

## Core Principles

### 1. Many Small Frequent Commits

Never batch changes into one large commit. Split work into logical
commits that are:
- Independently meaningful (one concern per commit)
- Compilable and testable (commits that would pass CI individually)
- No larger than ~300 lines unless the change is mechanically generated

Examples of good commit splits:
- `chore(deps): pin cdk-spilman to git rev bafc38f` (Cargo.toml + Cargo.lock only)
- `feat(spilman): typed SpilmanError enum with 7 variants` (one module)
- `test(spilman): mock-mint CI tests, no network required` (new test file)

### 2. Everything Is Pushed

**Unpushed work is lost work.** Every commit must be pushed to a remote
(c03rad0r fork on GitHub AND ngit) before the task is done.

- `git status` must show clean worktree
- `git push` must succeed with exit code 0
- If you can't push to ngit, push to GitHub at minimum and note ngit as blocked

### 3. Tightly Scoped PRs

Each PR must address exactly one concern. If a change touches more than
3 files or spans 500+ lines, it's probably too broad — split it.

Good scope examples:
- "Add CI workflow" — one file, one job
- "Pin cdk-spilman dependency" — one dep change + lockfile
- "Add SpilmanError enum" — one module, no logic changes

Bad scope examples:
- "Add spilman support" — mixes deps, types, tests, E2E
- "Fix everything" — never acceptable

### 4. Fork-First Stacking Strategy

All PRs live on c03rad0r's fork. The stack is structured as:

```
upstream/master (OpenTollGate/tollgate-rs)
└── PR A (lowest — targets upstream/master, lives on fork)
    └── PR B (targets PR A, fork only)
        └── PR C (targets PR B, fork only)
```

- Only the lowest PR in the stack targets upstream
- All higher levels stack on the fork
- Never open an upstream PR from this fork
- When the lowest PR is ready to go upstream, open it one at a time

### 5. Well-Structured PRs

Each PR must have:
- Clear title: `<type>(<scope>): <description>`
- Body explaining WHAT changed and WHY (not how)
- Reference to any related issues or upstream discussions
- CI status visible (all checks passing)

### 6. Dual Push: GitHub + ngit

Every commit pushed to GitHub must also be pushed to ngit. The ngit
remote uses the `nostr://` protocol with the configured nsec/npub.

```bash
# Push to GitHub
git push fork <branch> --no-verify

# Push to ngit
git push ngit <branch> --no-verify
```

If ngit push fails (repo not initialized on relay), initialize it:
```bash
# Requires interactive terminal for ngit init prompts
tmux new-session -d -s ngit-init \
  "cd /path/to/repo && /usr/local/bin/ngit -n '<nsec>' init --name '<name>' -i '<name>' -r wss://relay.ngit.dev; bash"
```

## Workflow

### Starting a New Task

1. Create a branch from the appropriate base
2. Split the work into 3-7 small commits
3. Push each commit individually (don't batch)
4. Create a PR on the fork with a clear title and body
5. Push to ngit

### Review

- Don't wait for reviewers — stack your PRs and keep building
- The stack unblocks automatically when lower PRs are reviewed
- Use integration branches to deliver working code while PRs are open
- Playwright videos for UI changes as review evidence

### CI

- CI runs on ALL branches (not just master)
- The CI workflow includes: format, clippy, build, test, spilman feature build
- E2E tests use a local in-memory test mint (no network)
- Spilman integration tests are marked `--ignored` (require testnut)
- Mock-mint tests are NOT ignored (run on every push)
- `cargo clean -p tollgate-net` when the git-rev pin changes (stale cache)
- `ulimit -n 65536` before cargo test (for ~250 rlibs)

## Pitfalls

- **Don't open PRs on OpenTollGate/tollgate-rs.** All PRs stay on the fork.
- **Pre-push hooks may block pushes.** Use `--no-verify` for fork pushes.
- **ngit requires interactive init.** The repo must be initialized on the relay
  before push. This is a one-time setup.
- **CI needs `protobuf-compiler`** — already in the workflow.
- **cargo check with spilman features** — must be tested separately from default.
