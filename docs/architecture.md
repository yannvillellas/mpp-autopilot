# Architecture and behaviour

## Runtime flow

1. Once a day, fetch the Ligue 1 clubs and matches from MPP.
2. Schedule three triggers per match: publication, T-60 minutes, and T-5
   minutes.
3. At each trigger, refresh the Elo ratings from Football-Data results,
   falling back to the last successful ratings when the fetch fails, then
   compute the best exact score and write it when needed.

## Elo ratings

Ratings are computed by the bot from the Ligue 1 results published by
[Football-Data](https://www.football-data.co.uk), the backtest source. Live odds
come from MPP. There is no third-party rating service: the rating is
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
the Elo cache answers with its real age, logged as `elo_source=cache`.

## Elo cache

Every successful ratings fetch is persisted in the state file. When a fetch
fails, the trigger pass reuses the last snapshot instead of failing:

- The snapshot is replaced after each successful fetch.
- Cached ratings are still used to compute and write predictions, and the
  trigger is marked as done.
- Every trigger log carries `elo_source` (`fresh` or `cache`) and
  `elo_age_seconds`. The fallback also logs the fetch error, `fetched_at` and
  the snapshot age.
- If the cached snapshot misses a club, that trigger stays pending so a later
  fetch can retry it; the other matches still run.
- Without a cached snapshot (first run), the trigger pass fails and the normal
  retry policy applies.

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
