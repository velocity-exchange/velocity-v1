#!/usr/bin/env bash
# Verify npm registry signatures (and, where publishers provide them, provenance
# attestations) for every third-party package this workspace resolves.
#
# `npm audit signatures` is npm-only -- bun and yarn do not implement it -- and
# it needs an npm-installed tree. This repo installs with bun, so the script
# builds a throwaway npm view of the same dependency set in a scratch directory
# and audits that. Nothing it does touches the real checkout.
#
# Two things make the npm view necessary rather than just running npm here:
#
#   1. npm does not understand bun's `workspace:` protocol, so the intra-repo
#      links are rewritten to `file:` paths. They resolve locally and are not
#      audited, which is correct -- they were never fetched from a registry.
#   2. The tree has peer-dependency conflicts bun tolerates and npm's resolver
#      refuses (anchor-bankrun and typedoc want older @coral-xyz/anchor and
#      typescript than the root pins). `--legacy-peer-deps` sidesteps them.
#      Resolution differences do not matter here: the audit asks "is the
#      published tarball signed by the registry", which is a property of each
#      package version, not of how it got selected.
#
# Usage: bash .github/scripts/npm-audit-signatures.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT

cd "$REPO_ROOT"

# Copy only the manifests. No sources, no lockfiles -- npm re-resolves from the
# declared ranges.
cp package.json "$SCRATCH/"
for dir in packages/* apps/*; do
	[ -f "$dir/package.json" ] || continue
	mkdir -p "$SCRATCH/$dir"
	cp "$dir/package.json" "$SCRATCH/$dir/"
done

cd "$SCRATCH"

node -e '
const fs = require("fs");
const path = require("path");

const manifests = ["package.json"];
for (const group of ["packages", "apps"]) {
  if (!fs.existsSync(group)) continue;
  for (const dir of fs.readdirSync(group)) {
    const p = path.join(group, dir, "package.json");
    if (fs.existsSync(p)) manifests.push(p);
  }
}

const location = new Map();
for (const f of manifests.slice(1)) {
  const j = JSON.parse(fs.readFileSync(f, "utf8"));
  if (j.name) location.set(j.name, path.dirname(f));
}

let rewritten = 0;
for (const f of manifests) {
  const j = JSON.parse(fs.readFileSync(f, "utf8"));
  for (const field of ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"]) {
    const deps = j[field];
    if (!deps) continue;
    for (const [name, range] of Object.entries(deps)) {
      if (typeof range !== "string" || !range.startsWith("workspace:")) continue;
      const target = location.get(name);
      if (!target) throw new Error(`workspace dep ${name} in ${f} has no local package`);
      deps[name] = "file:" + path.relative(path.dirname(f), target);
      rewritten++;
    }
  }
  fs.writeFileSync(f, JSON.stringify(j, null, 2));
}
console.log(`rewrote ${rewritten} workspace: ranges across ${manifests.length} manifests`);
'

npm install --package-lock-only --ignore-scripts --legacy-peer-deps --no-audit --no-fund
# audit signatures reads the installed tree, not just the lockfile.
npm ci --ignore-scripts --legacy-peer-deps --no-audit --no-fund

npm audit signatures
