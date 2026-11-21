# Shopping Sessions

A submission for the Databricks "Shopping Sessions" track of the Builders Buildathon. It cleans messy shopping logs, resolves linked user IDs into people, builds sessions, credits each session to an ad click, and writes two daily Parquet tables. `rules.md` is the specification. [NOTES.md](NOTES.md) numbers every rule (R1 to R40), and the code cites those numbers.

The submitted image (`Dockerfile`) is a single static Rust binary on an empty (`scratch`) base. A DuckDB implementation of the same rules (`pipeline/`, `Dockerfile.duckdb`) is the correctness oracle it is tested against.

## Approach (native binary, `src/`)

1. **Read.** Every input file is memory-mapped and cut into ~2 MB chunks on line boundaries. Chunks go to all cores through an atomic work queue, link files first.
2. **Parse.** A fast path (`src/fast.rs`) walks each line field by field in the expected shape:
   - IDs are packed into order-preserving 64-bit keys while scanning (6 bits per character, so numeric order equals byte order);
   - timestamps are parsed at fixed offsets with SWAR digit checks (`YYYY-MM-DDTHH:MM:SS`, offsets, 8-digits-per-multiply epoch milliseconds).

   Any line not exactly in that shape (a stray quote, an extra field, a bad timestamp, an ID that does not pack) bails to the generic splitter, which implements the dialect byte by byte, so the fast path cannot change a result. A randomized test checks this on 240,000 dirty lines. Records are compact: 32 bytes per event copy, with timestamps as 32-bit offsets. Nothing is normalized.
3. **Linked IDs.** The thread that finishes the last links chunk runs a union-find (path halving, union by size) while the others keep parsing. Each person is named by the byte-wise smallest of their IDs, including IDs that appear only in links. Shared devices need no special case: they are just IDs that link many people.
4. **Dedup.** Event copies are partitioned by an `event_id` hash. Each partition keeps the earliest-ingested copy (before validation), maps it to its person, and re-partitions survivors by a person hash. The dedup must be global because some duplicate copies name a different user.
5. **Sessions and attribution.** Per person, events are sorted by time and split on gaps over 30 minutes. The person's clicks are sorted by time and `click_id`. A sliding 7-day window gives each session its credit (latest click, then smallest `click_id`) and its set of assisting campaigns, with per-campaign counts.
6. **Write.** A small Parquet writer emits PLAIN, uncompressed data pages with an exact schema (`DATE` for `day`, strings as `UTF8`/`STRING`), one file per thread, written in parallel.

## Open-source libraries used

- [memmap2](https://crates.io/crates/memmap2) (MIT/Apache-2.0): memory-maps the input files. It is the native binary's only dependency.
- [Rust](https://www.rust-lang.org) standard library (MIT/Apache-2.0), built in the `rust:1.93-slim-bookworm` image.
- Oracle and tests only, not in the submitted image:
  - [DuckDB](https://duckdb.org) (MIT), the SQL engine of the reference pipeline;
  - [Apache Arrow / PyArrow](https://arrow.apache.org) (Apache-2.0);
  - [Polars](https://pola.rs) (MIT), used by the checker scripts.

## Run and test

```sh
# Native binary: unit tests, the 36 fixtures, the dev set
cargo test --release
cargo build --release
python scripts/run_fixtures.py fixtures -- target/release/shopping-sessions
DATA_DIR=dev OUTPUT_DIR=out target/release/shopping-sessions && python scripts/compare.py out dev-answer

# The image, run the way the scorer runs it (read-only root, no network, uid 1000, ...)
docker buildx build --platform linux/amd64 --load -t shopping-sessions:latest .
python scripts/run_fixtures.py fixtures -- python scripts/docker_run.py   # CPUS=8 MEM=24g by default
python scripts/docker_run.py dev out && python scripts/compare.py out dev-answer

# Differential test against the DuckDB oracle on generated datasets (needs the track's generator)
python scripts/difftest.py --gen path/to/generate --native target/release/shopping-sessions --seeds 1-12,15,62,249
```

`scripts/docker_run.py` is a cross-platform twin of `scripts/docker_run.sh`, with the same scorer flags. `scripts/bench.sh` times the image several times, and `SS_PROFILE=1` prints a per-stage time breakdown to stderr.

No data or answers are committed: `.gitignore` keeps generated datasets, `dev-answer/` and all outputs out of the repository.

## License

MIT, see [LICENSE](LICENSE).
