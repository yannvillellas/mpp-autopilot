# MPP Autopilot

MPP Autopilot is a Rust service that predicts Ligue 1 exact scores for
[Mon Petit Prono](https://mpp.football). It converts market odds from
[The Odds API](https://the-odds-api.com) into expected goals, falls back to its
own Elo ratings computed from
[Football-Data](https://www.football-data.co.uk) Ligue 1 results, combines the
probabilities with the MPP quotations, picks the score with the highest
expected points, and writes it to the MPP `general` contest.

## Quick start

```bash
cp .env.example .env   # set MPP_TOKEN and ODDS_API_KEY
cargo run -- list      # fixtures and quotations
cargo run -- once      # one daily pass (dry-run by default)
```

With Docker:

```bash
docker compose up -d --build
docker compose logs -f
docker compose run --rm mpp-autopilot check
```

The runtime image is distroless. Use `docker compose logs` or
`docker compose run --rm mpp-autopilot check` instead of a shell. `once` shares
the state file with the running service; stop the service or use a separate
state path/volume.

## Configuration

| Variable       | Default        | Description                                  |
| -------------- | -------------- | -------------------------------------------- |
| `MPP_TOKEN`    | —              | Auth0 token for the MPP app API.             |
| `ODDS_API_KEY` | —              | The Odds API key for Ligue 1 odds.           |
| `WRITE`        | `false`        | Set to `true` to submit predictions.         |
| `DAILY_HOUR`   | `7`            | Hour of the daily MPP fetch (Europe/Paris).  |
| `STATE_PATH`   | `./state.json` | State file (Docker uses `/data/state.json`). |
| `LOG_LEVEL`    | `INFO`         | `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR`.   |

`MPP_TOKEN` authenticates MPP Autopilot against the MPP app. `ODDS_API_KEY` is
required: the service refuses to start without it. `WRITE=false` runs the whole
pipeline without submitting anything.

## Commands

| Command               | Description                                      |
| --------------------- | ------------------------------------------------ |
| `mpp-autopilot`       | Run continuously.                                |
| `mpp-autopilot once`  | One daily fetch and the due triggers, then exit. |
| `mpp-autopilot list`  | Print the Ligue 1 fixtures and quotations.       |
| `mpp-autopilot check` | Validate the configuration and exit.             |

## Documentation

- [MPP API reference](docs/mpp-api.md)
- [Architecture and behaviour](docs/architecture.md)

## Development

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

## Disclaimer

Automated submissions may violate MPP's terms of service. Use at your own risk.
