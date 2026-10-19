# mpp

Autonomous Ligue 1 predictions for [Mon Petit Prono](https://mpp.football),
powered by [ClubElo](https://clubelo.com).

`mpp` reads the day's fixtures and quotations once, computes the exact score
that maximizes expected points, and submits it for you one hour and five
minutes before kickoff.

## Features

- One MPP request per day; ClubElo is refreshed at each trigger.
- Elo-driven Poisson model with a Dixon-Coles correction.
- Expected-value pick based on the MPP quotations.
- Dry-run by default; writing is opt-in.
- Persistent state, so restarts never duplicate a submission.
- Single Rust binary, Docker-ready.

## Quick start

```bash
cp .env.example .env   # set MPP_TOKEN
cargo run -- list      # fixtures and quotations
cargo run -- once      # one daily pass, no writes
```

With Docker:

```bash
docker compose up -d --build
docker compose logs -f
```

## Configuration

| Variable     | Default        | Description                                  |
| ------------ | -------------- | -------------------------------------------- |
| `MPP_TOKEN`  | —              | Auth0 access token for the MPP API.          |
| `WRITE`      | `false`        | Set to `true` to submit predictions.         |
| `DAILY_HOUR` | `7`            | Hour of the daily MPP fetch (Europe/Paris).  |
| `STATE_PATH` | `./state.json` | State file (Docker uses `/data/state.json`). |
| `LOG_LEVEL`  | `INFO`         | Optional log level.                          |

`WRITE=false` runs the whole pipeline — fetch, model, logs — without submitting
anything. Keep it for the first deployment and while testing changes.

## CLI

| Command     | Description                                      |
| ----------- | ------------------------------------------------ |
| `mpp`       | Run continuously.                                |
| `mpp once`  | One daily fetch and the due triggers, then exit. |
| `mpp list`  | Print the Ligue 1 fixtures and quotations.       |
| `mpp check` | Validate the configuration and exit.             |

## How it works

1. Once a day, fetch the Ligue 1 fixtures and quotations from MPP.
2. At publication, T-60 and T-5, refresh ClubElo and compute the best exact
   score.
3. Submit it to the MPP `general` contest, which also feeds private leagues.

The model turns the Elo gap into home/away expected goals, builds a 7×7 score
matrix, and picks the score with the highest expected points.

## Development

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

## Notes

- The MPP API is private and undocumented; endpoints may change without notice.
- Automated submissions may violate MPP's terms of service. Use at your own risk.
