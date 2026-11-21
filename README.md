# Shopping Sessions

A submission for the Databricks "Shopping Sessions" track of the Builders Buildathon. It cleans messy shopping logs, resolves linked user IDs into people, builds sessions, credits each session to an ad click, and writes two daily Parquet tables. `rules.md` is the specification. [NOTES.md](NOTES.md) numbers every rule (R1 to R40), and the code cites those numbers.

## Approach (current: DuckDB reference pipeline)

`pipeline/pipeline.py` implements the rules in order, mostly as DuckDB SQL:

1. **Raw lines, not DuckDB's CSV reader.** Each file is read as raw text (`read_text`) and split on `\n` only, and the CSV dialect (quotes, field counts, empty fields) is applied in SQL. DuckDB's CSV reader repairs quoting and reads `""` as `''`, which `rules.md` does not allow.
2. **Timestamps.** The three forms are matched with `[0-9]` regexes (never `\d`). The calendar is checked explicitly (month lengths, leap years), and epoch milliseconds come from integer arithmetic (Hinnant's `days_from_civil`).
3. **Linked IDs.** A union-find in Python with path compression and union by size. Each person is named by the byte-wise smallest of their IDs, including IDs that appear only in links.
4. **Events.** Dedup on `event_id` keeps the earliest-ingested copy before validation. Then come validation, the late-event filter, and sessionization with window functions (a new session after a gap over 30 minutes).
5. **Clicks.** Dedup and validation. Each session joins the person's clicks from the 7 days up to its start. Credit goes to the latest click, with ties to the smallest `click_id`; every distinct campaign among those clicks assists once.
6. **Outputs.** Both tables are written with `COPY ... (FORMAT parquet)`, with explicit `DATE` / `BIGINT` / `VARCHAR` types.

## Open-source libraries used

- [DuckDB](https://duckdb.org) (MIT): SQL engine.
- [Apache Arrow / PyArrow](https://arrow.apache.org) (Apache-2.0): hands the union-find result to DuckDB.
- Python 3.12 (`python:3.12-slim-bookworm` base image).
- Testing only, not in the image: [Polars](https://pola.rs) (MIT), used by the checker scripts.

## Run and test

```sh
pip install duckdb==1.5.6 pyarrow polars

# The 36 edge-case fixtures, without Docker
python scripts/run_fixtures.py fixtures -- python pipeline/pipeline.py

# The image, run the way the scorer runs it (read-only root, no network, uid 1000, ...)
docker buildx build --platform linux/amd64 --load -t shopping-sessions:latest .
python scripts/run_fixtures.py fixtures -- python scripts/docker_run.py   # CPUS=8 MEM=24g by default
python scripts/docker_run.py dev out && python scripts/compare.py out dev-answer
```

`scripts/docker_run.py` is a cross-platform twin of `scripts/docker_run.sh`, with the same scorer flags. It reads its limits from `CPUS` and `MEM`.

No data or answers are committed: `.gitignore` keeps the generated datasets, `dev-answer/` and all outputs out of the repository.

## License

MIT, see [LICENSE](LICENSE).
