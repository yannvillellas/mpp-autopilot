# Architecture and behaviour

## Runtime flow

1. Once a day, fetch the Ligue 1 clubs and matches from MPP.
2. Schedule three triggers per match: publication, T-60 minutes, and T-5
   minutes.
3. At each trigger, fetch every upcoming Ligue 1 event from The Odds API with
   one request. A fixture whose event has usable `h2h` and 2.5-goal `totals`
   odds is predicted from the market; otherwise the Football-Data Elo path
   answers, falling back to the last successful ratings when the fetch fails.
   The best exact score is then written when needed.

## Prediction sources

The source chain is evaluated per fixture:

1. market odds from The Odds API;
2. live Elo ratings computed from Football-Data results;
3. the last successfully fetched Elo snapshot, read from disk without network.

The market answers when the API returned an event for that fixture with at
least one complete bookmaker (both `h2h` and the 2.5-goal `totals` line). A
missing event, a failed request or an unusable book never fails the pass: the
fixture falls back to Elo. A failed Elo fetch still uses the cache; when
neither source can answer, the trigger stays pending and the normal retry
policy applies. A missing market therefore never costs a bet.

Every trigger log carries the source and its provenance:

| Field             | Meaning                                                         |
| ----------------- | --------------------------------------------------------------- |
| `source`          | `market`, `elo` or `elo_cached`.                                |
| `observed_at`     | Market observation time, or the time of the Elo fetch/snapshot. |
| `elo_age_seconds` | Age of the cached Elo snapshot; `0` for fresh or market data.   |

Market `observed_at` is the oldest market-level `last_update` among the selected
bookmakers. The market is fetched during the pass; its freshness is carried by
`observed_at`, and `source=market` says nothing about the price age. Cached Elo
is explicit in `source=elo_cached`; a fresh Elo fetch is `source=elo`. For a
fresh Elo fetch, `observed_at` is the pass time; for the cache it is the time of
the fetch that produced the snapshot.

## Market odds

The source is [The Odds API v4](https://the-odds-api.com/liveapi/guides/v4/):
one `GET /v4/sports/soccer_france_ligue_one/odds` request per trigger pass
with `regions=eu`, `markets=h2h,totals` and `oddsFormat=decimal`. The response
contains every upcoming Ligue 1 event, so a pass never makes one request per
match. The key is read from `ODDS_API_KEY`; the service refuses to start
without it. It is never logged or persisted, and request errors are stripped of
their URL before they reach a log line.

For each event, complete books are used as follows:

- Pinnacle is preferred when it provides both `h2h` and the 2.5-goal `totals`
  line.
- Otherwise the median of the margin-normalised probabilities across all
  complete books is used. Books without totals, without a 2.5 line or without
  complete `h2h` are ignored.
- An event must match the fixture's canonical clubs and be within six hours of
  its MPP kickoff; otherwise the fixture falls back to Elo.

The bookmaker margin is removed by dividing each implied probability by the
sum of the three implied probabilities. The over probability uses the same
normalisation for two outcomes:

```text
p_home = (1 / home) / (1 / home + 1 / draw + 1 / away)
pOver2.5 = (1 / over) / (1 / over + 1 / under)
```

The over probability pins the expected-goal total, because the sum of two
independent Poisson variables is Poisson:

```text
P(total > 2.5) = 1 - exp(-total) * (1 + total + total^2 / 2)
total = lambda_home + lambda_away        (1D solve)
```

The home share is then solved from the independent-Poisson home-win
probability at that fixed total:

```text
P(home win) = sum over h > a of Poisson(h; total * share) * Poisson(a; total * (1 - share))
lambda_home = total * share
lambda_away = total * (1 - share)
```

The resulting lambdas feed the existing expected-points model unchanged. Its
Dixon-Coles adjustment and MPP quotation layer are identical for market and
Elo inputs.

## Elo ratings

When the market cannot answer, ratings are computed by the bot from the Ligue 1
results published by [Football-Data](https://www.football-data.co.uk), the
backtest source. The MPP quotations remain the scoring layer; they are not a
probability source. There is no third-party rating service: the rating is
deterministic and owned by this project. On 612 out-of-sample matches it
scored a 1X2 log-loss of 0.9909 against 0.9950 for ClubElo.

The update rule is ClubElo's published method:

```text
expected = 1 / (1 + 10^(-((R_home + HFA - R_away) / SCALE)))
K        = BASE_K x margin(d)
R_home  += K x (S - expected)
R_away  -= K x (S - expected)
```

with `S = 1` for a home win, `0.5` for a draw and `0` for an away win, and
`d` the absolute goal difference. The margin multiplier is `1` for one goal,
`1.5` for two, and `(11 + d) / 8` from three goals up.

Every parameter is a named constant in `src/elo.rs` with its origin:

| Constant    | Value                  | Origin                                                          |
| ----------- | ---------------------- | --------------------------------------------------------------- |
| `SCALE`     | 400                    | Fixed by the Elo system: 400 points give 10:1 odds.             |
| `START`     | 1500                   | Elo convention for a club with no history.                      |
| `HFA`       | 65                     | ClubElo's published home advantage; the literature gives 60-80. |
| `BASE_K`    | 20                     | ClubElo's published adaptation rate.                            |
| goal margin | 1, 1.5, `(11 + d) / 8` | ClubElo's published margin multipliers.                         |

`HFA` and `BASE_K` were checked on Ligue 1 rather than tuned:

- Every home advantage from 40 to 90 gives a 0.9908-0.9910 log-loss because
  the calibration layer absorbs the shift; it only changes the rating scale.
- `K=10` gives 0.9902 against 0.9909 for `K=20`, a paired difference of
  0.0007 with a standard error of 0.0028 (noise). `K=30` (1.0033) and `K=40`
  (1.0800) degrade badly.

A club with no history starts at `START`; a relegated club keeps its rating so
it is still there if it comes back.

### Backfill and incremental updates

The first successful fetch backfills the archive from 2020/21 (about 2100
matches). The ratings, the applied matches and the last downloaded season are
persisted in the state. Later passes download the current season (and any
season missed while the service was down), apply only results that are not
already applied, and keep the rest of the history untouched. A restart
therefore resumes instead of replaying the backfill.

Results are sorted by their real match date before the ratings move, because
postponed fixtures are not in chronological order in the CSV. Applied matches
are identified by season, date and clubs, so a re-run of a matchday or a
restart in the middle of an update does not double count.

The CSV is a third-party file with quirks handled by the parser:

- the first header may carry a UTF-8 BOM;
- columns moved between seasons, so they are located by name;
- dates are `dd/mm/yyyy` (or `dd/mm/yy` in newer files);
- postponed fixtures have empty scores and are skipped, never treated as 0-0.

The current season's file is published a day or two after the matchday. That
only delays the newest results; it never fails a pass. A missing current-season
file is tolerated during the one-time backfill, where the archived seasons
still produce ratings. Once a backfill exists, the same 404 fails the fetch and
the Elo cache answers with `source=elo_cached` and its real age.

## Elo cache

Every successful ratings fetch is persisted in the state file. When a fetch
fails, fixtures without market odds reuse the last snapshot instead of
failing:

- The snapshot is replaced after each successful fetch.
- Cached ratings are still used to compute and write predictions, and the
  trigger is marked as done.
- For Elo answers, every trigger log carries `elo_age_seconds`, in addition
  to `source` (`elo` or `elo_cached`) and `observed_at`. The fallback also
  logs the fetch error, `fetched_at` and the snapshot age.
- If the cached snapshot misses a club, that trigger stays pending so a later
  fetch can retry it; the other matches still run.
- Without a cached snapshot (first run), the Elo path fails the pass and the
  normal retry policy applies, while any fixture served by market odds is
  still written.

There is no maximum age: Elo moves only when matches are played, and the age
stays visible in the logs.

## Idempotence

In write mode, every trigger reads the `general` forecast back from MPP
before writing:

- If MPP already holds the model pick, the trigger is marked as done without
  a write.
- If the remote forecast differs, the bot writes the model pick. A manual
  edit in the app is therefore corrected at the next trigger.
- If the read fails, the bot writes anyway. A wrong or missing score is worse
  than a redundant write.
- Dry-run never reads the forecast and never writes.

The local state is the scheduler memory and an audit trail. It is not the
source of truth for idempotence.

## Persisted state

State is written atomically to `STATE_PATH` with a temporary file and rename.

```json
{
  "mode": "write",
  "matches": {
    "mpp_championship_match_2640624": {
      "kickoff": "2026-10-23T18:45:00+00:00",
      "triggers": {
        "publish": "2026-10-19T05:00:00+00:00",
        "t60": "2026-10-23T17:45:00+00:00",
        "t5": "2026-10-23T18:40:00+00:00"
      },
      "last_written_score": "1-1",
      "last_written_at": "2026-10-19T05:00:01+00:00"
    }
  },
  "elo": {
    "fetched_at": "2026-10-19T05:00:00+00:00",
    "ratings": {
      "marseille": 1670.9520303480226,
      "psg": 1684.0983240112262
    },
    "applied": ["2026|2026-10-17|psg|marseille"],
    "backfilled_through": 2026
  }
}
```

`last_written_score` and `last_written_at` are an audit trail. Legacy keys
`score` and `written_at` are still read. Changing the write mode resets the
triggers but keeps the audit trail. The `elo` block is omitted until the first
successful fetch; `applied` and `backfilled_through` are omitted while they are
empty.
