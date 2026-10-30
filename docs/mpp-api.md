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
    "period": "preMatch",
    "matchStatus": "decided",
    "quotations": { "home": 81, "draw": 117, "away": 112 },
    "home": {
      "clubId": "mpp_championship_club_...",
      "score": 0
    },
    "away": {
      "clubId": "mpp_championship_club_...",
      "score": 0
    }
  }
}
```

Only Ligue 1 matches (`championshipId == 1`) with positive home, draw, and
away quotations are kept for predictions. Dates are UTC. The match object also
carries `home.score`, `away.score`, `period` (`preMatch`, `fullTime`, ...) and
`matchStatus` (`decided`, `postponed`, ...). The bot does not read results from
this payload: it uses the forecast history below, which also carries the MPP
points.

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
- After the match, the `general` forecast is purged (`general: null`). Results
  and MPP points must be read from the forecast history, not this route.

### `GET /user`

Returns the authenticated user. The bot reads `id` once and caches it in the
state; the history route needs it as `userId`.

```json
{
  "id": "user_...",
  "username": "yann"
}
```

### `GET /user-match-forecasts/championship/{championshipId}/history`

Returns the authenticated user's forecast history for one championship,
grouped by date. The web client calls this route on the Results screen.

Query parameters:

- `userId`: the id returned by `GET /user`.
- `limitDays=3`: each page covers three days.
- `beforeDate`: the `nextDate` returned by the previous page; omit on the
  first page.

Response shape:

```json
{
  "matchesByDate": {
    "2026-10-26": [
      {
        "matchId": "mpp_championship_match_...",
        "championshipId": 1,
        "date": "2026-10-26T18:45:00.000Z",
        "period": "fullTime",
        "matchStatus": "decided",
        "quotations": { "home": 48, "draw": 155, "away": 30 },
        "home": { "clubId": "mpp_championship_club_...", "score": 2 },
        "away": { "clubId": "mpp_championship_club_...", "score": 0 },
        "userForecast": {
          "homeScore": 2,
          "awayScore": 0,
          "points": {
            "base": 48,
            "exact": 30,
            "extra": 0,
            "bonus": 0,
            "total": 78,
            "rarityLevel": "rare"
          },
          "bonuses": { "doublePoints": 0 }
        }
      }
    ]
  },
  "beforeDate": "2026-10-26",
  "nextDate": "2026-10-23"
}
```

`nextDate` is the cursor for the next page; the route ends when it is absent
or equals the requested `beforeDate`. `period == "fullTime"` identifies a
finished match. `matchStatus` carries `postponed`, `abandoned`, `suspended` or
`delayed`; the bot never scores those from a partial result.

MPP returns the official points in `userForecast.points.total`; the bot stores
this total and also recomputes `base + exact` from the quotations and the 30
bonus. The two values are compared and a mismatch is logged, so the scoring
rule is verified against the app instead of trusted blindly. The bot reads one
page per daily pass and can walk up to
`MAX_REPORT_HISTORY_PAGES` (30 days) from the `score` command.

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
