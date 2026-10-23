# Architecture and behaviour

## Runtime flow

1. Once a day, fetch the Ligue 1 clubs and matches from MPP.
2. Schedule three triggers per match: publication, T-60 minutes, and T-5
   minutes.
3. At each trigger, refresh ClubElo, compute the best exact score, and write
   it when needed.

## Model

The Elo difference gives home and away expected goals. A 7x7 Poisson matrix
with a Dixon-Coles correction gives the probability of each exact score. The
pick is the score with the highest expected MPP points:

```text
EV(i, j) = P(outcome(i, j)) * odds[outcome(i, j)] + p_exact(i, j) * 30
```

`WRITE=false` computes and logs the same pick without writing.

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
  }
}
```

`last_written_score` and `last_written_at` are an audit trail. Legacy keys
`score` and `written_at` are still read. Changing the write mode resets the
triggers but keeps the audit trail.
