---
'@velocity-exchange/admin-cli': patch
---

`--dry-run` now reports the size of the proposal transaction against the 1232-byte limit, and says how far over it is when a batch will not fit. Previously an oversized batch compiled and dry-ran cleanly, then failed only at propose time with `Transaction too large`. The estimate builds the same instructions and memo the real dispatch uses, since the memo is stored inline and affects the size.
