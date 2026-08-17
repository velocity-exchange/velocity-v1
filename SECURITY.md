# Security Policy

**DO NOT CREATE A GITHUB ISSUE** to report a security problem.

Email security@velocity.exchange with a detailed description of the attack vector. For critical and
high severity bugs, we require a proof of concept done on a privately deployed mainnet program.

Velocity's bug bounty program — severity tiers, payouts, submission process, and what is out of
scope — is documented at
<https://docs.velocity.exchange/protocol/risk-and-safety/bug-bounty>.

For the protocol's external trust surface — every CPI target, oracle, whitelisted venue, and the
full transitive crate graph, each with its trust assumption and failure mode — see
[docs/EXTERNAL-DEPENDENCIES.md](./docs/EXTERNAL-DEPENDENCIES.md).

Past audits are listed in [AUDIT.md](./AUDIT.md).
