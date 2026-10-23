# MPP API reference

> MPP's API is private and undocumented. This is a reverse-engineering note,
> not an official contract. Endpoints, headers, and payloads may change
> without notice.

## Base URL and headers

```text
https://api.mpp.football
Authorization: Bearer <MPP_TOKEN>
platform: web
client-language: fr-FR
accept: application/json
application: mppLfp
```

The shipped web client sends `application: mppLfp` as a header. This project
also sends `?application=mppLfp` on every request.

## Endpoints

### `GET /championship-clubs`

Returns the club directory.

```json
{
  "championshipClubs": {
    "mpp_championship_club_198": {
      "id": "mpp_championship_club_198",
      "name": { "fr-FR": "Paris SG", "en-GB": "Paris SG" },
      "shortName": "PSG"
    }
  }
}
```

The bot resolves a club name from `name["fr-FR"]`, then `name["fr"]`, then
`shortName`.

### `GET /championships-current-matches`

Returns current matches keyed by match id.

```json
{
  "mpp_championship_match_2640624": {
    "matchId": "mpp_championship_match_2640624",
    "championshipId": 1,
    "gameWeekNumber": 8,
    "date": "2026-10-23T18:45:00.000Z",
    "quotations": { "home": 81, "draw": 117, "away": 112 },
    "home": { "clubId": "mpp_championship_club_..." },
    "away": { "clubId": "mpp_championship_club_..." }
  }
}
```

Only Ligue 1 matches (`championshipId == 1`) with positive home, draw, and
away quotations are kept. Dates are UTC.

### `GET /user-match-forecasts/{matchId}`

Returns the forecasts registered for one match.

```json
{
  "general": {
    "homeScore": 1,
    "awayScore": 1,
    "editedAt": "2026-10-23T18:30:00.000Z",
    "points": {
      "base": 9,
      "exact": 3,
      "extra": 0,
      "bonus": 0,
      "total": 12
    }
  },
  "mpp_challenge_...": { "homeScore": 1, "awayScore": 1 }
}
```

The `points` object carries `base`, `exact`, `extra`, `bonus`, and `total`;
the values above are illustrative.

Observed responses:

- A registered forecast has a `general` object and may include other entity
  keys.
- A purged forecast (after the match) answers `{ "general": null }`.
- An unknown match id answers `{}`.
- `404` has not been observed on this route; it is logged as a warning and
  treated as "no forecast".
- A non-empty payload without `general` is treated as a read failure, not as
  "no forecast"; the bot logs a warning and writes.

### `PATCH /user-match-forecasts/entity/general/match/{matchId}`

Writes the exact score for the `general` contest.

```json
{
  "homeScore": 1,
  "awayScore": 1,
  "originPage": "matchSheet"
}
```

Writing to `general` also feeds the user's private leagues. From kickoff
onwards, the API answers `403` with `matchForecastsNotAllowed`; the lock is
enforced server-side.

## Authentication

`MPP_TOKEN` is an Auth0 access token. It is read from the environment or
`.env` and must never be logged or committed. A `401` means the token was
rejected; renew it and restart the process.

## Error handling

- `401`: authentication failure. A write retries after the token is renewed;
  a forecast read fails open.
- `5xx`, `429`, and network timeouts: on a write, transient and retried with
  the shared backoff; on a forecast read, fail open immediately without a
  retry.
- Other `4xx` on a write: non-transient; the trigger is marked without a
  write.
- A failed forecast read is never retried: the bot writes the computed score
  immediately. A wrong or missing score is worse than a redundant write.
