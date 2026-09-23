# Isomorphic code

Some features you might want to add to the SDK only work in one execution environment, usually node
rather than the browser. This folder holds the shims that keep those features out of the browser
build without breaking the types or the developer experience on the SDK side. Follow the existing
`grpc`, `anchor` and `anchor29` files when you add a new one.

## How the separation works

The compiled `.js` in a browser bundle must not pull in a package that cannot run there. A package
gets pulled in when you import a class, method, or other value from it. Importing only its types does
not, because the types disappear at compile time. So each isomorphic package gets three source files
and a build-time swap:

- `<pkg>.ts` is the surface the rest of `src/` imports, for example `../isomorphic/anchor`. It
  re-exports the node implementation, so typechecking and the node build see the real thing.
  `grpc.ts` re-exports `./grpc.node`; `anchor.ts` and `anchor29.ts` re-export `@coral-xyz/anchor` and
  `@coral-xyz/anchor-29` directly, which is what their `.node.ts` files do too.
- `<pkg>.node.ts` holds the node implementation.
- `<pkg>.browser.ts` holds the browser implementation. Where a feature cannot work in the browser,
  throw from it so the consumer gets a clear message instead of a bundling failure. `grpc.browser.ts`
  throws `Only available in node context` from `createClient`, and `anchor.browser.ts` throws from
  the `AnchorProvider` and `Program` constructors, pointing the caller at `VelocityCore`.

`tsc -p tsconfig.json` emits `lib/node` and `tsc -p tsconfig.browser.json` emits `lib/browser`. Both
still contain `isomorphic/<pkg>.js` compiled from `<pkg>.ts`, meaning the node implementation.
`scripts/postbuild.js` then overwrites `lib/<env>/isomorphic/<pkg>.js` and `<pkg>.d.ts` with the
`<pkg>.<env>.js` and `<pkg>.<env>.d.ts` output, and deletes the other environment's files from that
directory.

Only what `<pkg>.browser.ts` exports survives into the browser build. `grpc.browser.ts` defines
`createClient` and nothing else, so the rest of `grpc.node.ts`'s exports, `CommitmentLevel` and
`Client` among them, are absent there.

## Adding an isomorphic package

1. Create `[your-package-name].ts`, `[your-package-name].node.ts` and
   `[your-package-name].browser.ts`.
2. Remove direct imports of the incompatible library from the rest of `src/`. Everything goes through
   your isomorphic files instead.
3. Import types with `import type { ... }`, not `import { ... }`, so no value reference survives
   compilation. `grpc.node.ts` does this for `Client`, `SubscribeRequest` and `SubscribeUpdate`.
4. Put the concrete classes, functions and constants in the `.node` and `.browser` files. Export them
   however suits the package, as long as the two files agree on the names.
5. For a dependency that may be missing even in node, load it lazily with `await import()` and expose
   async getters, as `grpc.node.ts` does for the optional `helius-laserstream`. Enum-like values that
   callers read synchronously can be backed by a `Proxy` returning the known numeric values, which is
   how `LaserCommitmentLevel` and `CompressionAlgorithms` work.
6. Add the package name to `isomorphicPackages` in `scripts/postbuild.js`.
7. Run `bun run build`, which produces the node files in `lib/node` and the browser files in
   `lib/browser`. `bun run build:browser` passes `--force-env browser` to the postbuild script, which
   puts the browser files in both output directories.
