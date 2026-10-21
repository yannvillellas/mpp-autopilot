# MPP Autopilot

MPP Autopilot is an autonomous Ligue 1 prediction bot for
[MPP (Mon Petit Prono)](https://mpp.football), powered by
[ClubElo](https://clubelo.com).

The bot reads the day's MPP fixtures and quotations once, computes the exact
score that maximizes expected points, and submits it when the match is first
seen, one hour before kickoff, and five minutes before kickoff.

## Features

- One MPP snapshot per day (two GETs); ClubElo is refreshed at each trigger.
- Elo-driven Poisson model with a Dixon-Coles correction.
- Expected-value pick based on the MPP quotations.
- Dry-run by default; writing is opt-in.
- Persistent state prevents completed triggers from being replayed across
  restarts.
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
docker compose run --rm mpp-autopilot check
```

The runtime image is distroless: there is no shell inside the container.
Use `docker compose logs`, `docker compose run --rm mpp-autopilot check`, or a
temporary debug container. A dry-run smoke test is:

```bash
WRITE=false docker compose run --rm mpp-autopilot once
```

`once` shares `STATE_PATH` with the service. Stop the service or use a
separate state path/volume before running it against a live deployment.

## Configuration

| Variable     | Default        | Description                                  |
| ------------ | -------------- | -------------------------------------------- |
| `MPP_TOKEN`  | —              | Auth0 token for the MPP app API.             |
| `WRITE`      | `false`        | Set to `true` to submit predictions.         |
| `DAILY_HOUR` | `7`            | Hour of the daily MPP fetch (Europe/Paris).  |
| `STATE_PATH` | `./state.json` | State file (Docker uses `/data/state.json`). |
| `LOG_LEVEL`  | `INFO`         | `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR`.   |

`MPP_TOKEN` authenticates MPP Autopilot against the MPP app. The other variables
configure the bot itself.

`WRITE=false` runs the whole pipeline — fetch, model, logs — without submitting
anything. Keep it for the first deployment and while testing changes.

## CLI

| Command               | Description                                      |
| --------------------- | ------------------------------------------------ |
| `mpp-autopilot`       | Run continuously.                                |
| `mpp-autopilot once`  | One daily fetch and the due triggers, then exit. |
| `mpp-autopilot list`  | Print the Ligue 1 fixtures and quotations.       |
| `mpp-autopilot check` | Validate the configuration and exit.             |

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
- A crash between a successful PATCH and the state write is the residual
  duplicate-submission window; reading forecasts back is out of scope.
