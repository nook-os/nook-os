# hack/

One-off, developer-specific scripts and tasks — the stuff that helps *us* run
the project but isn't part of the product or its test suite.

Contrast with `scripts/`, which holds the supported, checked-in tooling every
contributor relies on (`gen-types.sh`, `check-release-version.sh`, the e2e
harness). Things here are more operational and more personal: deploy helpers,
data pokes, migration one-shots. They may assume a particular host, SSH alias,
or local setup.

- **`release-to-crimson.sh`** — deploy a published release to the prod control
  plane on crimson (`./hack/release-to-crimson.sh [vX.Y.Z] [-y] | --rollback`).
- **`scratch/`** — throwaway working area. Everything in it is git-ignored;
  drop temporary files, dumps, and experiments there instead of in the repo
  root. Nothing in `scratch/` is ever committed.
