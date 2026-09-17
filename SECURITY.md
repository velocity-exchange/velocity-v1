# Security policy

**Do not create a GitHub issue to report a security problem.**

Email security@velocity.exchange with a detailed description of the attack vector. A critical or
high severity report must include a proof of concept run against a privately deployed mainnet
program.

The bug bounty program is documented at
<https://docs.velocity.exchange/protocol/risk-and-safety/bug-bounty>. That page holds the severity
tiers, the payouts, the submission process, and what is out of scope.

[docs/EXTERNAL-DEPENDENCIES.md](./docs/EXTERNAL-DEPENDENCIES.md) records the protocol's external
trust surface. It lists every CPI target, oracle, and whitelisted venue, plus the full transitive
crate graph, each with its trust assumption and failure mode.

Past audits are listed in [AUDIT.md](./AUDIT.md).
