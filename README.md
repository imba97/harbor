# harbor

[![crates.io](https://img.shields.io/crates/v/harbor?style=flat-square&logo=rust)](https://crates.io/crates/harbor)
[![docs.rs](https://img.shields.io/docsrs/harbor?style=flat-square&logo=docs.rs)](https://docs.rs/harbor)
[![CI](https://img.shields.io/github/actions/workflow/status/imba97/harbor/ci.yaml?style=flat-square&logo=github)](https://github.com/imba97/harbor/actions/workflows/ci.yaml)
[![MSRV](https://img.shields.io/badge/MSRV-1.75-blue?style=flat-square)](https://blog.rust-lang.org/2023/12/28/Rust-1.75.0.html)
[![licence](https://img.shields.io/crates/l/harbor?style=flat-square)](#licence)

**Release orchestration for a Cargo workspace.** Work out what publishes in what order,
check a release before it starts, and drive the registry.

[中文文档](README_CN.md)

## Features

- 🧮 **Derived order** — topological sort of the dependency graph, nothing to keep in sync
- 🛑 **Preflight** — cycles, `publish = false` clashes and broken `members` fail before any
  upload
- 🏷️ **Tag check** — tag against manifest version; missing token told apart from empty
- 🔁 **Resumable** — a version already on the registry counts as done, so a re-run finishes a
  partial release
- ⏱️ **Retries that matter** — an unindexed dependency is waited for, a bad token is not
- 🧪 **Testable** — every decision goes through a `CommandRunner`, no registry needed
- 📚 **Library first** — the CLI is a thin shell over a public API

## Install

```console
$ cargo install harbor --locked        # the `cargo harbor` subcommand
```

## Command

```console
$ cargo harbor plan
5 crate(s) to publish, in this order:
   1. acme-core 0.4.0
   2. acme-macros 0.4.0
   3. acme-net 0.4.0
   4. acme-cli-lib 0.4.0
   5. acme-app 0.4.0

1 not published (publish = false):
  - acme-cli 0.4.0

$ cargo harbor check v0.4.0           # tag vs manifest version, and a token is present
$ cargo harbor check v0.4.0 --plan    # ...and the plan is computable at all
$ cargo harbor publish                # upload, in that order
$ cargo harbor publish --dry-run      # say what would happen
```

That order is nothing but the dependency graph: `acme-core` and `acme-macros` depend on
nothing so they go first (in name order, so the output is stable), `acme-app` waits for
everything. `--plan` is worth using in CI — computing the plan is offline and free, and it is
the only step that catches a cycle or a broken `members` pattern.

| Option | Meaning |
| --- | --- |
| `--root <dir>` | Workspace root (default `.`) |
| `--order <crate>` | Publish exactly these crates, in this order (repeatable, comma-separated) |
| `--prefix <p>` | Only crates starting with `p` are candidates (default: no filter) |
| `--no-locked` | Allow the release to change the lockfile (default: refuse) |
| `--wait <s>` | Delay between index-propagation retries (default 10) |
| `--attempts <n>` | Retry budget for index propagation (default 60) |
| `check --plan` | Also compute the plan |
| `publish --dry-run` | Print what would happen, upload nothing |

## Library

```rust
use harbor::{Release, ReleaseConfig};

let release = Release::new(".").with_config(ReleaseConfig::default());

for krate in &release.plan()?.order {
    println!("{} {}", krate.name, krate.version);
}

let report = release.publish()?;
println!("{} published", report.published.len());
```

`Publisher::with_runner` takes a `CommandRunner`, which is what makes a release testable
without a registry.

## In CI

```yaml
- run: cargo install harbor --locked

- name: Publish
  run: |
    # The tag check only makes sense on a tag; `publish` is safe to run anywhere because it
    # fails before uploading if the plan cannot be computed.
    if [ "$GITHUB_REF_TYPE" = "tag" ]; then cargo harbor check --plan "$GITHUB_REF_NAME"; fi
    cargo harbor publish
  env:
    # Read from the environment, never from an argument list: a token on a command line is
    # visible to every process on the machine.
    CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
```

See [`.github/workflows/`](.github/workflows/) for how this repository releases itself.

## `--order`, when the derivation is wrong

By default the release is derived from the manifests, which needs no configuration — even
for a workspace of one.

Use `--order` when a crate should ship but its manifest says `publish = false`, a combination
that happens for real: a tool installed as a binary declares the flag so nothing can make it
a dependency, and that same flag stops it releasing itself. Turning the flag off instead
would publish every other crate in the workspace.

```console
$ cargo harbor --order some-tool publish
```

`--order` selects and sequences, but it does not override the graph: naming a crate before
something it depends on is refused, not obeyed. Names are *package* names, not binary names —
the package is `some-tool`, its binary is `cargo-some-tool`.

## What it does not do

No version bumping, no changelog, no git tags, no binary packaging. Those are decisions a
project makes for itself.

## Licence

MIT.
