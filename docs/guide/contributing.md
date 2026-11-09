# Contributing

PuntoHunter takes bug reports, ideas, and pull requests. This page says how to send each, and what a change
needs before it is merged. [Development](development.md) covers building, running, and testing.

## Reporting a Bug

Open an issue on GitHub with:

* the version, from `puntohunter --version`, and the network: mainnet or regtest;
* what you did, what you expected, and what happened instead;
* the log lines around it, and the config sections involved.

Leave secrets out of anything you post: the node's RPC password or cookie, the data directory's key files
(`handle.key`, `sv2-authority.key`), balance tokens, and fleet passwords. A payout address is public, but
you may still prefer to shorten it.

## Reporting a Vulnerability

Never in an issue. Report it privately through the repository's Security tab, with GitHub's private
vulnerability reporting, so it can be fixed before it is known. That covers anything that could cost a miner
or an operator money, expose an address or a secret, or let a client disrupt a pool.
[SECURITY.md](../../SECURITY.md) gives the scope and the supported versions.

## Proposing a Change

Pull requests are welcome. For anything beyond a small fix, such as a typo, a broken link, or a clear bug
with a short fix, open an issue first, so the idea is agreed before the code is written. A change that is
not wanted, or wanted another way, costs everyone less as an issue than as a finished pull request.

Keep one topic to a pull request. Say what it changes and why, and how you tested it.

## What a Change Includes

* **Tests.** A fix comes with a test that fails without it. New behaviour is tested where it lives: a unit
  test beside the code, or an end-to-end test in [tests/e2e/](../../tests/e2e/) for behaviour across the
  binary, the node, and miners.
* **The guide.** A change to a setting, an endpoint, a Stratum option, or a component updates its page of
  this guide, operator or architecture, in the same pull request.
* **The changelog.** A change a user or an operator can see adds an entry at the top of
  [CHANGELOG.md](../../CHANGELOG.md), under an `## Unreleased` heading, which the first change after a release
  adds, in the style of the entries there. A schema change gives the SQL that
  brings an existing database up to date.
* **The checks.** Formatting, clippy, the tests, the end-to-end suite, and markdownlint pass, as
  [Before You Send a Change](development.md#before-you-send-a-change) lists. CI runs all but the end-to-end
  suite on every pull request; say in the pull request that you ran it, and how it went.
* **The project's rules**, in [AGENTS.md](../../AGENTS.md). A change written with an AI agent is welcome and
  keeps the same rules; the agent reads them there.

## Review

Every change is reviewed before it is merged. A change to how shares are credited, how a block's reward is
split, or how payouts are made gets the closest look: miners' money depends on it. Expect questions, and
requests for tests, on those.

## License

PuntoHunter is [MIT licensed](../../LICENSE). By sending a contribution you agree that it is licensed under
the same terms, and that you have the right to send it. There is no separate agreement to sign.
