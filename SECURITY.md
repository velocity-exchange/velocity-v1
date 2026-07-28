# Bug Bounty Overview
**DO NOT CREATE A GITHUB ISSUE** to report a security problem.

Velocity offers bug bounties for Velocity's on-chain program code and its web application; web application bugs are capped at the High bounty tier.

These are guidelines for bug severity. Each bug bounty submission will be evaluated on a case-by-case basis.

### Critical
**Payout:** 10% of the value of the hack, with a minimum of $50,000 and a maximum of $500,000

Example impacts:
- Direct theft of a significant amount of user funds without preconditions
- Permanent freezing, even after a program upgrade, of a significant amount of user or protocol funds
- Direct theft of a significant amount of protocol funds or protocol insolvency

### High
**Payout:** $10,000 to $50,000 per bug

Example impacts:
- Theft of user funds with preconditions
- Theft of protocol-held assets with preconditions
- User or protocol funds remain frozen after a program upgrade when specific preconditions are met

### Medium
**Payout:** $1,000 to $5,000 per bug

Example impacts:
- Temporary freezing of funds
- Denial of service issues that can be resolved with an upgrade
- Theft of a small amount of funds, or theft requiring significant preconditions

### Low
**Payout:** $1,000 to $5,000 per bug

Other issues that may not qualify for one of the above tiers.

## Submission
Please email hello@drift.trade with a detailed description of the attack vector. For all bugs, we require a proof of concept. We will reach back out in 3 business days with additional questions or the next steps on the bug bounty.

### Duplicate Reports
Compensation for duplicative reports will be split among reporters with first to report taking priority using the following equation:

R: total reports
ri: report priority
bi: bounty share

bi = 2 ^ (R - ri) / ((2^R) - 1)
#### Bounty Split Examples
| total reports | priority | share  |
| ------------- | -------- | -----: |
| 1             | 1        | 100%   |
| 2             | 1        | 66.67% |
| 2             | 2        | 33.33% |
| 3             | 1        | 57.14% |
| 3             | 2        | 28.57% |
| 3             | 3        | 14.29% |
| 4             | 1        | 53.33% |
| 4             | 2        | 26.67% |
| 4             | 3        | 13.33% |
| 4             | 4        |  6.67% |
| 5             | 1        | 51.61% |
| 5             | 2        | 25.81% |
| 5             | 3        | 12.90% |
| 5             | 4        |  6.45% |
| 5             | 5        |  3.23% |

## Bug Bounty Payment
Bug bounties will be paid in USDC. Alternative payment methods can be used on a case-by-case basis.

## Invalid Bug Bounties
The following are out of scope for the bug bounty:
1. Attacks that the reporter has already exploited themselves, leading to damage.
2. Attacks requiring access to leaked keys/credentials.
3. Attacks requiring access to privileged addresses (governance, admin).
4. Incorrect data supplied by third party oracles (this does not exclude oracle manipulation/flash loan attacks).
5. Lack of liquidity.
6. Third party, off-chain bot errors (for instance bugs with an arbitrage bot running on the smart contracts).
7. Best practice critiques.
8. Sybil attacks.
9. Attempted phishing or other social engineering attacks involving Velocity contributors or users
10. Denial of service, or automated testing of services that generate significant traffic.
11. Any submission violating [Immunefi's rules](https://immunefi.com/rules/) 
