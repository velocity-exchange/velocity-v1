# Releases, npm packages and Docker images

Read this before you add a changeset, change a publish workflow, add an app, or cut a release.

## Changesets

Library packages under `packages/*` publish through
[changesets](https://github.com/changesets/changesets), not release-please (removed). Add a
changeset in your PR with `bun run changeset`. Merging the auto-maintained "Version Packages" PR
commits the version bumps. Then push a tag `npm-<pkg>-v<version>` to trigger
`.github/workflows/npm-publish.yml`, which builds and publishes that one package through npm OIDC
trusted publishing. `<pkg>` is the directory name under `packages/`.

| Package                         | Tag example             |
| ------------------------------- | ----------------------- |
| `@velocity-exchange/sdk`        | `npm-sdk-v0.2.3`        |
| `@velocity-exchange/admin-cli`  | `npm-cli-admin-v0.2.3`  |
| `@velocity-exchange/vaults-sdk` | `npm-vaults-sdk-v0.2.3` |

The tag version must match the `package.json` version set by the "Version Packages" PR. The
workflow skips the publish if that version is already on the registry. Publishing uses `npm`, not
`bun`, because bun does not implement npm's OIDC trusted-publishing flow.
`.github/scripts/rewrite-workspace-deps.mjs` rewrites workspace dependency ranges to concrete
versions before publish.

A PR that changes user-facing behavior in a publishable package includes a changeset. That covers
new features, bug fixes and API changes, but not chores, CI config, or internal refactors that do
not affect consumers. Run `bun run changeset` at the repo root, select the affected packages,
choose the bump type, write a short description, and commit the generated `.changeset/*.md` file.
Never edit `package.json` versions by hand. Changesets and the "Version Packages" bot own those
fields.

**One changeset per feature branch.** While a branch is unmerged, it carries exactly one `.changeset/*.md` file; every later change on the branch folds into that file in place. The changeset becomes the published release notes, and a consumer only ever sees the branch's final surface — so rewrite it to describe that final surface, and delete anything an intra-branch change superseded ("X was renamed to Y" is noise when X never shipped). Never add a second changeset for the same branch.

## Release CLI

`bun run release <status|bump|devnet|npm|docker|infra|mainnet>` (`deploy-scripts/release.sh`)
pushes the tags, dispatches the workflows and updates the infra pins above, in release order. It is
read-only until `--execute` is passed. See the "Release CLI" section of
[`../../deploy-scripts/README.md`](../../deploy-scripts/README.md). When a tag convention, workflow
name or publish path changes in `.github/workflows/`, update the script in the same change.

## Apps and Docker images

`apps/*` are the deployable services, all `private` and never published to npm: `dlob-server`,
`keeper-bots-v2` and `usermap-server`. The infrastructure-v3 services (`candles`, `market-data`,
`multisig-monitor`, `notification-engine`, `realtime-archiver`, `aggregator-api`) and their
`@backend/*` support libraries are not part of this monorepo. They deploy from `infrastructure-v3`.

Pushing a git tag `docker-<app>-v<version>` triggers `.github/workflows/velocity-publish.yml`,
which builds that app's image and pushes it to ECR (eu-west-1, through OIDC). `docker-info.json`
maps each app to its build metadata (path, turbo scope, output dir or entrypoint or cargo bin, ECR
repo). TS apps build through `docker/ts-app.Dockerfile` (full-context bun and turbo). Rust apps
(`keep-rs`, `swift`) build through `docker/rust-app.Dockerfile`. The version is everything after
the last `-v`, so app keys may contain `-v` (for example `docker-keeper-bots-v2-v1.4.2`). To add a
new app, add a `docker-info.json` entry.
