---
'@velocity-exchange/sdk': patch
---

Publish only the built `lib/` output (adds a `files` field, shrinking the npm tarball from ~14 MB unpacked to the compiled artifacts), widen the `engines` constraint from `^24.0.0` to `>=20` so Node 20/22 LTS consumers install without engine errors, and point the `repository` field at the public https URL.
