---
'@velocity-exchange/admin-cli': patch
---

Harden `wallet swap` against a compromised swap API: every instruction returned by the swap-instructions endpoint is validated against a program allowlist (Jupiter v6, SPL Token, Token-2022, Associated Token Program, System Program) and rejected if it requires a signature from any account other than the owner. The instruction program list is printed before signing or proposing so reviewers see what is actually being signed.
