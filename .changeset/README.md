# Changesets

This monorepo uses [changesets](https://github.com/changesets/changesets) to version and publish
the library packages under `packages/*`: `@velocity-exchange/sdk`, `@velocity-exchange/admin-cli`,
`@velocity-exchange/vaults-sdk` and `@velocity-exchange/jit-proxy`. Apps under `apps/*` are
`private` and ship as Docker images instead, routed by `docker-info.json` and
`.github/workflows/velocity-publish.yml`.

Workflow:

1. In a PR that changes a publishable package, run `bun run changeset` and describe the bump.
2. On merge to `master`, the `changesets` workflow opens or updates a **Version Packages** PR that
   runs `changeset version`, which bumps the versions and writes the CHANGELOGs. Merge it to commit
   the bumps.
3. Push a per-package tag `npm-<pkg>-v<version>`, where `<pkg>` is the directory name under
   `packages/`. That publishes that one package through `.github/workflows/npm-publish.yml`, using
   npm OIDC trusted publishing. The tag version must match the `package.json` version the "Version
   Packages" PR committed. The workflow is idempotent and skips the publish when that version is
   already on the registry.

Tag examples: `npm-sdk-v0.2.3`, `npm-cli-admin-v0.2.3`, `npm-vaults-sdk-v0.2.3`,
`npm-jit-proxy-v0.2.3`. The version is everything after the last `-v`, so a package directory name
may itself contain `-v`.
