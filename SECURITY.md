# Security policy

Do not open a GitHub issue to report a security problem.

Email security@velocity.exchange with a detailed description of the attack. For critical and high
severity bugs we require a proof of concept run against a privately deployed mainnet program.

Velocity's bug bounty program documents the severity tiers, payouts, submission process, and what
is out of scope:
<https://docs.velocity.exchange/protocol/risk-and-safety/bug-bounty>.

[docs/EXTERNAL-DEPENDENCIES.md](./docs/EXTERNAL-DEPENDENCIES.md) lists everything the protocol
trusts outside its own code: every CPI target, oracle, whitelisted venue, and the full transitive
crate graph, each with its trust assumption and failure mode.

Past audits are listed in [AUDIT.md](./AUDIT.md).
