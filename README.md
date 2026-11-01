# MPP Autopilot

MPP Autopilot is a Rust service that predicts Ligue 1 exact scores for
[Mon Petit Prono](https://mpp.football). It converts market odds from
[The Odds API](https://the-odds-api.com) into expected goals, falls back to its
own Elo ratings computed from
[Football-Data](https://www.football-data.co.uk) Ligue 1 results, combines the
probabilities with the MPP quotations, picks the most probable score of the
most profitable outcome, and writes it to the MPP `general` contest.

## Quick start

```bash
cp .env.example .env   # set MPP_TOKEN and ODDS_API_KEY
cargo run -- list      # fixtures and quotations
cargo run -- once      # one daily pass (dry-run by default)
cargo run -- score     # season score and results
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
required for the service, `once` and `check`; `list` and `score` only need
`MPP_TOKEN`. `WRITE=false` runs the whole pipeline without submitting anything.

## Results and season score

The daily pass reads one page of the MPP forecast history and keeps a season
ledger next to the forecasts: pick, final score, MPP points, the quotation used
by the model, the source and the points the modal score would have earned.
Results are read-only and never influence a forecast, a trigger or a write.

`mpp-autopilot score` loads the ledger without changing the service mode,
refreshes up to 30 days of history, and prints one line per match with the
season total and the modal-score counterfactual. A match without a result is
printed with its MPP status (`postponed`, `delayed`, ...) instead of being
silently omitted.

## Commands

| Command               | Description                                        |
| --------------------- | -------------------------------------------------- |
| `mpp-autopilot`       | Run continuously.                                  |
| `mpp-autopilot once`  | One daily fetch, results refresh and due triggers. |
| `mpp-autopilot list`  | Print the Ligue 1 fixtures and quotations.         |
| `mpp-autopilot check` | Validate the configuration and exit.               |
| `mpp-autopilot score` | Print the season score and refresh results.        |

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
