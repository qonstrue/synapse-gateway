# Contributing to synapse-gateway

Thank you for your interest in contributing to synapse-gateway! This is an open-source,
AGPL-3.0-licensed project and we welcome issues, bug reports, feature requests, and pull
requests from the community.

Before you begin, please read our [Code of Conduct](CODE_OF_CONDUCT.md) and
[Security Policy](SECURITY.md). By participating you agree to abide by the Code of Conduct.

---

## Table of Contents

1. [Getting started](#getting-started)
2. [Before you submit](#before-you-submit)
3. [Development workflow](#development-workflow)
4. [Commit messages](#commit-messages)
5. [Developer Certificate of Origin (DCO)](#developer-certificate-of-origin-dco)
6. [Pull requests](#pull-requests)
7. [License](#license)

---

## Getting started

### Prerequisites

- [Rust stable toolchain](https://rustup.rs/) (the version pinned in `rust-toolchain.toml`
  or the current stable release)
- `cargo fmt` and `cargo clippy` (both ship with `rustup`)
- Optional: Docker (for the Dockerfile-based integration environment)

### Clone and build

```bash
git clone https://github.com/sustentabilitas/synapse-gateway.git
cd synapse-gateway

# Default build (features: server + ledger-sqlite)
cargo build

# Run the test suite
cargo test
```

### Feature matrix

synapse-gateway has a rich feature matrix. When your change touches a ledger backend or the
embeddable library surface, build and test the relevant variants:

| Command | What it enables |
|---|---|
| `cargo build` | Default features (`server` + `ledger-sqlite`) |
| `cargo build --features ledger-postgres` | PostgreSQL cost-ledger sink |
| `cargo build --features ledger-pubsub` | Google Cloud Pub/Sub ledger sink |
| `cargo build --features ledger-sns` | AWS SNS ledger sink |
| `cargo build --features "ledger-pubsub ledger-sns"` | Both cloud ledger sinks together |
| `cargo build --no-default-features --lib` | Lean embeddable core (no HTTP server, no ledger) |

---

## Before you submit

Run all of the following locally before opening a pull request. CI enforces each gate and
a red build blocks merging.

```bash
# 1. Format check (must produce no diff)
cargo fmt --all --check

# 2. Lint (zero warnings, warnings treated as errors)
cargo clippy --all-targets -- -D warnings

# 3. Test suite with default features
cargo test

# 4. Build all feature variants your change touches
cargo build --features ledger-postgres
cargo build --features ledger-pubsub
cargo build --features ledger-sns
cargo build --features "ledger-pubsub ledger-sns"
cargo build --no-default-features --lib
```

Also:

- **Update `CHANGELOG.md`**: add a line under the `## [Unreleased]` section describing your
  change under the appropriate heading (`Added`, `Changed`, `Fixed`, `Removed`, `Security`).
- **Update documentation**: if your change affects public API, configuration, or CLI flags,
  update the relevant docs (README, files under `docs/`, or inline rustdoc).

---

## Development workflow

Non-trivial contributions — new features, significant refactors, new ledger backends, changes
to the public library API — should follow the **spec → plan → implement** flow used in this
project.

1. **Write a spec** under `docs/specs/`. Describe *what* and *why*: the problem,
   proposed behaviour, edge cases, and acceptance criteria. Keep it concise.

2. **Write a plan** under `docs/plans/`. Break the work into small, reviewable
   steps. The plan references the spec.

3. **Implement with TDD**: write the failing test first (in `tests/` or as a `#[cfg(test)]`
   module in the relevant source file), then write the minimal implementation that makes it
   pass, then refactor. Commit the test separately from the implementation where it aids
   reviewability.

4. **Open a PR** that links to the spec and plan documents so reviewers have full context.

Small bug fixes and documentation improvements do not need a full spec; use your judgement.

---

## Commit messages

- Use an **imperative, present-tense subject line** (e.g. `add Pub/Sub ledger sink`, not
  `added` or `adding`).
- Keep the subject under **72 characters**.
- Use conventional-commit prefixes when applicable:

  | Prefix | Use for |
  |---|---|
  | `feat:` | New feature or behaviour |
  | `fix:` | Bug fix |
  | `docs:` | Documentation changes only |
  | `refactor:` | Code restructuring without behaviour change |
  | `test:` | Adding or updating tests |
  | `chore:` | Maintenance, dependency updates, tooling |
  | `perf:` | Performance improvements |
  | `ci:` | CI/CD pipeline changes |

- Optionally include a body (blank line after subject) explaining *why* the change was made.
- Reference related issues or PRs at the bottom: `Closes #42`.

Example:

```
feat: add AWS SNS ledger sink

Adds a fan-out sink that publishes cost-ledger events to an SNS topic.
Gated behind the `ledger-sns` feature flag.

Closes #17
Signed-off-by: Your Name <your@email.com>
```

---

## Developer Certificate of Origin (DCO)

**Every commit must carry a `Signed-off-by` trailer.**

By signing off you certify that you have the right to submit the contribution under the
project's AGPL-3.0 license, as defined by the Developer Certificate of Origin at
<https://developercertificate.org/>.

Add the sign-off automatically with the `-s` flag:

```bash
git commit -s -m "feat: your change description"
```

This appends a line like:

```
Signed-off-by: Your Name <your@email.com>
```

The name and email must match a real identity (your real name and a working email address).

> **Pull requests that contain unsigned commits will not be merged.** If you forget to sign
> off on earlier commits, you can amend them:
>
> ```bash
> # Amend the most recent commit
> git commit --amend -s --no-edit
>
> # Sign off all commits in the branch at once (rebase approach)
> git rebase --signoff HEAD~<N>
> ```

---

## Pull requests

1. **Fork** the repository and create a feature branch from `main`.
2. Follow the [development workflow](#development-workflow) and
   [commit guidelines](#commit-messages).
3. Ensure **all CI checks pass** before requesting review.
4. Open a pull request with:
   - A clear title (conventional-commit style).
   - A description explaining *what* changed and *why*.
   - Links to the relevant spec/plan docs for non-trivial changes.
   - `Closes #<issue>` if applicable.
5. Address reviewer feedback promptly. One approving review from a maintainer is required
   to merge.
6. Maintainers may squash or rebase on merge to keep the history clean.

---

## License

By submitting a contribution you agree that your work is licensed under the
[GNU Affero General Public License v3.0](LICENSE) (AGPL-3.0), the same license as the rest
of the project.

If you have any questions, feel free to open a discussion or reach out via the contact in
[SECURITY.md](SECURITY.md).
