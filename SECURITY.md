# Security Policy

## Reporting a Vulnerability

Report a vulnerability privately, through this repository's **Security** tab ("Report a vulnerability"),
with GitHub's private vulnerability reporting. Never report one in an issue, a pull request, or a
discussion: those are public. There is no email address for reports.

Say what is affected, how to reproduce it, and what it lets someone do. A report is acknowledged, and its
fix is released before the vulnerability is described in public.

## Supported Versions

The latest release is supported. A fix goes into the next release, not into older ones.

## Scope

In scope is anything in this repository that could cost a miner or an operator money, expose a payout
address or a secret, or let a client disrupt a pool:

* Stratum V1 and V2: sessions, share validation, and the authority key;
* the coinbase and the split of a block's reward, PPLNS, the ledger, and payouts;
* the JSON API under `/api/v1`, its limits, and what it reveals;
* the web portal.

Out of scope: a pool's own deployment (its host, node, proxy, and firewall), Bitcoin Core, and the
dependencies' own vulnerabilities, which go to their projects. The guide's
[security architecture](docs/guide/architecture/security.md) describes the trust boundaries.
