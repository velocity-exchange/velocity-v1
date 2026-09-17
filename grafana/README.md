# Quoter health dashboard and alerts

A PropAMM quoter is an arbitrary program that the on-chain router invokes by CPI. A quoter that
reverts fails the whole simulation, so one bad entry stops a market's book from publishing and
turns a route request into an error. The router routes around such an entry and records what it
observed. These files read that record.

## What is measured, and where it comes from

A quoter's failures are mostly invisible to the chain. The router simulates before it sends, so a
failing quoter fails the simulation and the transaction never lands. Nothing is archived and no
event fires. The measurement therefore happens inside the processes that simulate.

| Source | Sees |
| --- | --- |
| `book-publisher` | Every registered quoter, every tick, on every market. It simulates whether or not a taker is routing, so it is the health probe. |
| `swift` `/route` | Quoters carried on real route requests. |
| `keep-rs` filler | Execute legs that break a real fill. It reports a failure but excludes no entry. A signed route is enforced on chain, so dropping an entry the taker named replaces one rejection with another. |

Attribution is positive. A simulation that carries several quoters can fail for a reason that
belongs to none of them, such as the taker's own margin, a stale oracle, or an account the builder
left out. A quoter is therefore charged only when the evidence names it. An unnamed failure
increments `quoter_sim_failures_unattributed_total`, which measures the router's blind spot rather
than a maker's behaviour. A rise in that counter means attribution is missing a case.

## Loading

The dashboard expects a Prometheus data source and reads it from a `datasource` variable, so it
works against any Prometheus instance.

```
# Grafana: Dashboards -> New -> Import -> Upload quoter-health.json
# Prometheus: add to rule_files in prometheus.yml
rule_files:
  - quoter-health.rules.yml
```

Scrape targets: `book-publisher` serves `/metrics` on `METRICS_ADDR`, default `0.0.0.0:9464`.
`keep-rs` and `swift` export on the ports they already use.

## Runbook

Automatic degradation reverses itself. A quarantine expires into probation, clean traffic promotes
a quoter back, and the backoff resets after a period with no failure. A single quarantine needs no
action.

Two cases need a person.

**A repeat offender.** `QuoterBanCandidate` fires when the router quarantined the same quoter three
times in a day. Automatic handling has already given it several chances. Pull the on-chain approval
so every router drops it:

```
velocity-admin quoter set-approved <quoter> false
```

That takes a warm admin key. To hold a quoter off one router without touching the chain, pin it:

```
curl -XPOST <publisher>/quoters/<quoter>/pin \
  -d '{"admission":"denied","reason":"broken rollout","expiresInSeconds":86400}'
```

**Rolling a pin back.** A pin sits in a layer the scorer never writes, so the computed state
survives underneath it. Clearing the pin resumes automatic handling at once, and rebuilds nothing:

```
curl -XDELETE <publisher>/quoters/<quoter>/pin
```

Set `expiresInSeconds` on every pin. `QuoterPinStale` fires after a week, because an override with
no end turns a temporary decision into a permanent one.

## When a maker ships a fix

No action is needed. The router watches each quoter's program account. A new deploy slot drops the
counters, because the old numbers describe code that no longer runs. The quoter then enters
probation rather than full flow, and clean traffic promotes it from there. A program that serves
many registry entries moves all of them, because the upgrade changed every tenant's behaviour.

## Endpoints

| Endpoint | Purpose |
| --- | --- |
| `GET /metrics` | Prometheus scrape. |
| `GET /quoters` | Every quoter's admission, cause, and current rates. |
| `GET /quoters/transitions` | Audit trail, with the numbers behind each move. |
| `POST /quoters/{quoter}/pin` | Override. Body: `admission`, `reason`, optional `sampleRate`, `expiresInSeconds`, `actor`. |
| `DELETE /quoters/{quoter}/pin` | Clear the override. |
