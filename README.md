# harbor

Release orchestration for a Cargo workspace: work out what publishes in what order, check
a release before it starts, and drive the registry.

`harbor` is a library first and a command second. The command is a thin shell over the
library, so everything a release decides is testable without a registry, a token, or a CI
run — which is the point, because the decisions are where releases go wrong.

## Why it exists

Publishing a workspace has one correct shape and several ways to get it silently wrong:

- **A crate cannot be published before its dependencies.** `cargo publish` resolves each
  dependency against the *live* registry, so the order is a topological sort of the
  dependency graph. A hand-maintained copy of that order is a bug waiting to happen: the
  project this was extracted from had one, it listed a crate before something it depended
  on, and the publish loop retried the impossible dependency for ten minutes before
  reporting a problem with the registry index. Nothing in that failure pointed at the list.
- **Which crates ship is a property of each crate.** `publish = false` in the crate's own
  manifest, not a list of exceptions beside a workflow that drifts out of step with it.
- **A partial release is normal.** crates.io never allows a re-upload, so a re-run has to
  recognise what is already there and continue.
- **An index that has not caught up is a matter of time; a bad token is not.** Retrying both
  wastes ten minutes to produce the same failure, so only one of them is retried.

## Using the library

```rust
use harbor::{Release, ReleaseConfig};

fn main() -> Result<(), harbor::Error> {
    // Every crate in the workspace is a candidate by default. `crate_prefix` narrows that
    // when one workspace holds several families of crates.
    let release = Release::new(".").with_config(ReleaseConfig::default());

    let plan = release.plan()?; // reads the workspace, nothing else
    for krate in &plan.order {
        println!("{} {}", krate.name, krate.version);
    }

    let report = release.publish()?; // uploads, in that order
    println!("{} published", report.published.len());
    Ok(())
}
```

`Publisher::with_runner` takes a `CommandRunner`, so the whole publishing policy — the
order, re-run behaviour, retry budget, and the exact commands issued — can be exercised
from a test with no registry involved.

## Using the command

The binary is named `cargo-harbor`, so Cargo finds it as a subcommand:

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

`check --plan` is worth its own line: computing the plan is offline and free, and it is the
only thing that catches a dependency cycle, a published crate that depends on an excluded
one, or a `members` pattern that matches nothing — all of which would otherwise surface half
way through a release. The preflight runs before anything is uploaded either way; `--plan`
is for the case where you want the *whole* question answered before the tag is pushed.

That order is a topological sort: `acme-core` and `acme-macros` depend on nothing, so they
go first (in name order, because ties are broken by name to keep the output stable);
`acme-net` and `acme-cli-lib` wait for those; `acme-app` waits for everything. Nothing is
configured — it falls out of the manifests.

| Option | Meaning |
| --- | --- |
| `--root <dir>` | Workspace root (default `.`) |
| `--prefix <p>` | Only crates starting with `p` are candidates (default: no filter) |
| `--no-locked` | Allow the release to change the lockfile. Off by default |
| `--wait <s>` | Delay between index-propagation retries (default 10) |
| `--attempts <n>` | Retry budget for index propagation (default 60) |
| `check --plan` | Also compute the plan, so an unpublishable workspace fails at `check` |

## What it deliberately does not do

No version bumping, no changelog generation, no git tags, no binary packaging. Those are
decisions a project makes for itself. This crate answers one question — what publishes, in
what order, and did it work — and answers it well enough to run unattended.

## In CI

```yaml
- name: Install the release tool
  # Once harbor is on crates.io; use `cargo install --path <checkout>` to build it from a
  # local copy instead.
  run: cargo install harbor --locked

- name: Check the tag names this version
  if: github.event_name == 'push'
  run: cargo harbor check "$GITHUB_REF_NAME"
  env:
    CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}

- name: Publish in dependency order
  run: cargo harbor publish
  env:
    # The token is read from the environment, never from an argument list: a token on a
    # command line is visible to every process on the machine, and one in a log is leaked
    # to everyone who can read the run.
    CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
```

### How this repository releases itself

`.github/workflows/ci.yaml` is a reusable workflow — it has no trigger of its own and can
only be called — and `release.yaml` calls it, so "do not publish until everything passes" is
`needs:` rather than a matter of timing. Tag pushes (`v*.*.*`) are the only trigger.

The publish job builds this crate from the tagged commit and uses it for its own preflight,
which makes the release a real exercise of the tool. It then publishes with `cargo publish`
rather than `harbor publish`, and that is deliberate: this crate's manifest sets
`publish = false` — correct for a tool that is installed with `cargo install` and has no
business being a dependency — and `harbor publish` honours that flag, so it would refuse to
publish itself. The same flag is why there is no `harbor plan` step there: a plan for a
workspace whose only crate is excluded is an empty plan, which proves nothing.

## Testing

```console
$ cargo test
```

No network, no token, no registry. The unit tests cover the graph, the ordering, the
manifest reading (including the two spellings of an inherited version), the outcome
classification against output recorded from real `cargo publish` failures, and the whole
publish policy through a fake runner.

## Licence

MIT.
