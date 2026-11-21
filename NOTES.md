# NOTES: Shopping Sessions

Phase 0 output. `rules.md` is the source of truth: where this file disagrees with it, this file is wrong.

Sources read: `rules.md`; the DuckDB starter (README, Makefile, Dockerfile, run.sh, pipeline.py, scripts/\*); all 36 fixtures and `fixtures/README.md`; the generator source (`main.rs`, `plan.rs`, `user.rs`, `text.rs`, `random.rs`); the dev set I generated (seed 1, 1M events, 250k clicks) and `dev-answer.zip`. The Polars and PySpark starters have identical scripts, Makefile and fixtures; only their READMEs differ.

## 1. Rules, numbered

Pipeline code cites these as `R<n>`.

### Inputs

- **R1** `$DATA_DIR` holds `events/*.csv` (`event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents`), `clicks/*.csv` (`click_id,user_id,campaign_id,ts`), `links/*.csv` (`user_id_a,user_id_b`), `products.csv` (`product_id,category`), `campaigns.csv` (`campaign_id,channel`) and `manifest.json` (`window_start`, `window_end` as F1 strings, different for every dataset).
- **R2** Read every `*.csv` in `events/`, `clicks/` and `links/`. The file count and sizes vary: a few large files, many small ones, and 1 to 3 per folder that hold only the header.
- **R3** Every CSV file has a header row. Skip the first line of each file. Every later line is data, even one equal to the header (fixture L3).
- **R4** `products.csv` and `campaigns.csv` are clean and their keys are unique. Campaign IDs look like `c_123`.

### CSV dialect

- **R5** Files are UTF-8. Only `\n` ends a line. A `\r` is an ordinary character of the field it sits in (E20: a price `1000\r` is bad; an event ID `e3\r` is not `e3`).
- **R6** Every comma separates fields. No value contains a comma, a quote or a newline.
- **R7** Any field may be wrapped in double quotes. The quotes are syntax: `"u1"` is `u1`, and `""` is an empty field.
- **R8** An empty field is null. Nothing in the rules tells null apart from an empty string.
- **R9** A malformed line is dropped. A line is malformed if any of these holds:
  - it has the wrong number of fields (events 7, clicks 4, links 2);
  - it has a quote anywhere except a pair around a whole field;
  - it is empty.
- **R10** Spaces are part of the value, quoted or not. Nothing is trimmed or case-folded (see R36).
- **R11** "Digits" means ASCII `0` to `9` only (E18: `٢٥٠` and `２５０` are not digits).

**The field rule I'll implement.** Split the line on every `,`. Then each field must be one of two things:

- a field with no `"` at all, whose value is the field itself;
- a field of length at least 2 that starts and ends with `"` and has no `"` inside, whose value is the inside.

Any other field makes the whole line malformed. A value of `""` is null.

### Timestamps and windows

- **R12** `ts` in events and clicks has three valid forms. Each converts to integer milliseconds since the epoch, UTC:
  - **F1** `YYYY-MM-DDTHH:MM:SSZ`.
  - **F2** `YYYY-MM-DDTHH:MM:SS±HH:MM`. The offset's hours are 00 to 23 and its minutes 00 to 59, with a colon. UTC is local time minus the offset, and `-00:00` is UTC.
  - **F3** One or more digits: epoch milliseconds. Leading zeros are allowed.
- **R13** Anything else is a bad timestamp:
  - empty;
  - fractional seconds;
  - a lowercase `t` or `z`, or a space instead of `T`;
  - no zone;
  - any non-digit in F3 (`1791071890000.0`, `1.79e12`, `-5`);
  - an impossible calendar value (month 13, Feb 30, Feb 29 outside a leap year, hour 24, second 60);
  - an offset out of range (`+24:00`, `+05:60`) or without its colon (`+0200`).
- **R14** A very short F3 value (`86400000`) or one too long for a 64-bit integer is not malformed. It is outside the window, and so bad (E18).
- **R15** `ingest_ts` is always valid F1.
- **R16** Event window: `window_start <= ts < window_end`.
- **R17** Click window: `window_start - 7 days <= ts < window_end`, where 7 days is 604,800,000 ms. A `ts` outside its window is bad.

### Linked IDs

- **R18** Parse: drop malformed lines and lines where either field is empty.
- **R19** Every remaining line joins its two IDs into one person. Joining is transitive across all lines, files and orders. A repeated line (either way round) or a self-link changes nothing. IDs compare exactly.
- **R20** A person is named by the byte-wise smallest of their IDs. That includes IDs seen only in links (L2: `U7` names a person who never uses it). An ID in no remaining line is a person on its own.
- **R21** Links come before everything else and have no time. An event or click belongs to its ID's person as soon as it is parsed.

**There is no special shared-device rule in `rules.md`.** A shared device's ID is an ordinary ID that links everyone who used it, so they all become one person: "thousands of people, interleaved sessions" in the generator's words. The correct handling is plain transitive closure (union-find). Do not cap component size, skip high-degree IDs, or limit propagation depth: chains run 50 to 400 links long.

### Events, in this order

- **R22** Parse: drop malformed lines and lines with an empty `event_id`.
- **R23** Dedup on `event_id` across all files, keeping the copy with the smallest `ingest_ts`. Copies with equal `ingest_ts` are identical.
  - Dedup comes before validation. If the kept copy is invalid, the event is dropped, and a later valid copy does not rescue it (E5).
  - A malformed copy was dropped in R22, so it is never the kept copy.
- **R24** Validate the kept copy. Keep it only if all of these hold:
  - `user_id` is non-empty;
  - `ts` is valid and in the event window;
  - `event_type` is exactly `view`, `cart` or `purchase`;
  - `product_id` is non-empty;
  - for a `purchase` only: `price_cents` is digits only, with a value from 1 to 10,000,000 (`0250` is 250).

  For `view` and `cart`, `price_cents` is ignored whatever it holds.
- **R25** Drop late events: drop the event if `ingest_ts - ts > 86,400,000` ms. Exactly 24 h is kept, and so is an `ingest_ts` before `ts`.
- **R26** Sessionize each person's surviving events, all IDs together, sorted by `ts`. A new session starts when `ts - previous ts > 1,800,000` ms. Equal timestamps always share a session. A session starts at its first event's `ts`.
- **R27** A session's day is the UTC date of its start. All its events and purchases count on that day, even those after midnight.
- **R28** Each product's category comes from `products.csv` by exact `product_id`. A product not in the file has the category `unknown`, which competes like any other category.

### Clicks

- **R29** Parse: drop malformed lines and lines with an empty `click_id`.
- **R30** Dedup on `click_id`. Copies have identical values, so keep any one.
- **R31** Validate: keep a click only if `user_id` is non-empty, `ts` is valid and in the click window, and `campaign_id` is exactly in `campaigns.csv`.
- **R32** Credit each session to one click. The candidates are the person's valid clicks, from any of their IDs, with `click.ts <= start` and `start - click.ts <= 604,800,000`. Take the latest `ts`; on a tie, the byte-wise smallest `click_id`.
- **R33** A session with no qualifying click is credited to campaign `organic`, channel `organic`. That ID never appears in `campaigns.csv`.
- **R34** One click can credit many sessions. Clicks never create, split or merge sessions.
- **R35** Assists: every distinct campaign with at least one qualifying click (the same set R32 chooses from, including the chosen one) assists the session once. `organic` never assists.
- **R36** Strings are never normalized: case-sensitive and untrimmed everywhere.

### Outputs

- **R37** Layout:
  - Two folders, `$OUTPUT_DIR/user_daily/` and `$OUTPUT_DIR/campaign_daily/`, each with at least one `*.parquet` directly inside it. There are no partition subfolders.
  - Files starting with `.` or `_`, other files and symlinks are ignored. Row order and file count don't matter.
  - Exactly the listed columns, in any order, with no extras. Any integer width and any string encoding are accepted, but `day` must be a Parquet `DATE`.
- **R38** `user_daily` has one row per person and day with at least one session starting that day.
- **R39** `campaign_daily` has one row per `(campaign_id, day)` with at least one session credited to or assisted by that campaign. An assist-only row has 0 in the three credited columns. `organic` rows exist only where `organic` was credited.
- **R40** Self-check: summed `sessions` and summed `revenue_cents` are equal in the two tables. Summed `assisted_sessions` is at least the summed `sessions` of the non-`organic` rows.

## 2. Exact output schema

This is what we will write. The types match the starter's: DuckDB `BIGINT` / `VARCHAR` / `DATE`.

"Nullable" below means whether a value can be null. In the Parquet files, DuckDB marks every column `OPTIONAL`; the native writer will mark only `top_category` `OPTIONAL`. The checker accepts any integer width and either repetition.

`user_daily/`

| Column          | Physical     | Logical         | Nullable | Definition                                                                                       |
| --------------- | ------------ | --------------- | -------- | ------------------------------------------------------------------------------------------------ |
| `user_id`       | `BYTE_ARRAY` | `STRING` (UTF8) | no       | The person's name (R20)                                                                           |
| `day`           | `INT32`      | `DATE`          | no       | UTC date of the session start (R27)                                                               |
| `sessions`      | `INT64`      | signed int      | no       | Sessions starting that day                                                                        |
| `events`        | `INT64`      | signed int      | no       | Surviving events in those sessions, all types                                                     |
| `revenue_cents` | `INT64`      | signed int      | no       | Sum of purchase `price_cents` in those sessions; 0 if none                                        |
| `top_category`  | `BYTE_ARRAY` | `STRING`        | **yes**  | The category with the highest summed purchase price over those sessions. Ties go to the byte-wise smallest name; null if there are no purchases. |

`campaign_daily/`

| Column                | Physical     | Logical    | Nullable | Definition                                                                    |
| --------------------- | ------------ | ---------- | -------- | ----------------------------------------------------------------------------- |
| `campaign_id`         | `BYTE_ARRAY` | `STRING`   | no       | The campaign, or `organic`                                                    |
| `channel`             | `BYTE_ARRAY` | `STRING`   | no       | From `campaigns.csv`; `organic` for `organic`                                  |
| `day`                 | `INT32`      | `DATE`     | no       | UTC date of the session start                                                 |
| `sessions`            | `INT64`      | signed int | no       | Sessions credited to the campaign that started that day                       |
| `purchasing_sessions` | `INT64`      | signed int | no       | Of those, the sessions with at least one purchase                             |
| `revenue_cents`       | `INT64`      | signed int | no       | Sum of purchase `price_cents` in those sessions                               |
| `assisted_sessions`   | `INT64`      | signed int | no       | Sessions starting that day that the campaign assisted; 0 for `organic`        |

- **Sizes.** `organic` revenue for one day at 60M events can approach 2^31, so `revenue_cents` stays `INT64`. Counts would fit `INT32`.
- **Files.** One or more files per table, with no partitioning, named like `part-0.parquet`. An empty table still gets a zero-row file (see section 3).
- **The dev answer** is CSV with a header, `day` as `YYYY-MM-DD`, null `top_category` as an empty field, and no quoting. For seed 1 the window runs from 2027-12-09 to 2027-12-23.
  - `user_daily` has 117,038 rows. 96,338 have a null `top_category` and 181 have upper-case names.
  - `campaign_daily` has 27,400 rows, 14 of them `organic` (one per day).

## 3. How the checker compares

This comes from `scripts/compare.py` and `scripts/run_fixtures.py`.

- **Loading.** It reads every `*.parquet` directly in the table folder (non-recursive), skipping names that start with `.` or `_` and symlinks. A folder with no Parquet file is an error. So an empty table must still be written as a zero-row file: fixtures E5 and E13 expect empty tables.
- **Types.** The column set must equal the schema exactly, in any order.
  - Integers: any polars integer type, cast to `Int128` for comparison.
  - Strings: `String`, `Categorical` or `Enum`, cast to `String`.
  - `day` must read back as polars `Date` exactly, which means a Parquet `DATE`. Anything else raises.
- **`compare.py` (check-dev).** It anti-joins both ways on all columns with `nulls_equal=True` and counts duplicate keys on `(user_id, day)` and `(campaign_id, day)`. It reports MATCH only when both anti-joins and both duplicate counts are 0.
  - The comparison is exact equality. There are no float columns, so no tolerance exists or is needed. Row order doesn't matter.
- **`run_fixtures.py`.** It runs the command with `DATA_DIR`/`OUTPUT_DIR` set in the environment and also passed as the last two arguments. It sorts the rows by their string form and compares the lists exactly.
- **The hidden scorer** reports only the exit code, OOM, and missing/extra/differing row counts.

## 4. Generator facts that matter

### Correctness

- **Event duplicates.** Copies are ingested 1 to 600 s *after* the original, so the original always has the strictly smallest `ingest_ts`. Equal-`ingest_ts` ties happen only among later copies, which are identical.
- **Changed user on duplicates.** For 1 in 64 duplicated events whose original line survived, the later copies name a different user (the next person's first ID). So event dedup must be global, not per person: the earliest copy's user wins (E4).
- **Bad original.** For 2% of duplicated events only the original has a bad `ts`, so the event is dropped (E5).
- **Malformed faults:**
  - drop the last field (6 fields);
  - append `,x` (8 fields);
  - add a leading `"`;
  - add a trailing `"`;
  - empty the first field (an empty `event_id` or `click_id`, or an empty link side);
  - add an extra blank line, while still writing the original line.

  Dev counts for events: 91 lines with 6 fields, 82 with 8, and 90 empty lines.
- **Clicks.**
  - Copies have identical values, quoted differently.
  - 1% come as two clicks at the same millisecond, so the `click_id` tie-break matters.
  - Some sit exactly 7 days, 7 days + 1 ms, 0 ms and −1 ms before a session start.
- **Links.**
  - 8% are repeated, either way round, and 1% of people have a self-link.
  - An empty side (0.2%) drops the line.
  - 1 in 2,000 sides is upper-cased. That makes a new ID which sorts before every lower-case ID, so it names its person and can split a person in two.
- **Shared devices.** 3 to 6 device IDs, each linking 0.4% of people. At 60M events that is about 1.3k to 2.7k real people per person, with tens of thousands of events. Whales and chain people can be in a shared device's person too.
- **Chains.** 3 people, plus 1 in 20,000 of the rest, have 50 to 400 IDs linked in random-order chains.
- **Whales.** 5 people, each with 1 to 2% of all events and a session in every one of the 168 two-hour slots.
- **Timestamps.**
  - Valid F3 never has leading zeros.
  - F1 and F2 appear only for whole seconds.
  - Offsets run from −10:00 to +12:00.
  - Bad `ts` values include valid-looking times just outside the window (`window_start - 1 ms`, `window_end`).
- **IDs.**
  - User IDs are `u` plus base36 (9 characters or fewer). Event IDs are `e…`, click IDs `k…`, product IDs `p<n>`, campaign IDs `c_<n>`.
  - Case variants exist: `U…`, `E…`, `K…`, `P<n>`, `C_<n>`.
  - IDs never contain spaces, quotes, `\r` or non-ASCII bytes. I checked the dev set: 0 CR bytes, 0 non-ASCII lines.
- **Window.** A random midnight from 2025 to 2028, 14 days long. Some windows contain 2028-02-29, and the bad Feb 29 uses a non-leap year.

### Scale

The dev set (seed 1) has:

- events: 30 files, 78.7 MB, about 1.07M lines;
- clicks: 40 files, 11.5 MB, about 255k lines;
- links: 14 files, 0.9 MB, about 45k lines.

The generator writes it in 1.7 s.

Extrapolating linearly, 20M events comes to about 1.6 GB of events plus about 0.23 GB of clicks, or roughly 1.85 GB of CSV. That assumes clicks stay at events/4, which nobody has confirmed. 60M comes to about 5.5 GB. `user_daily` should be about 2.3M rows at 20M and about 7M at 60M.

## 5. Derived facts

These are safe simplifications for the native version, each to be confirmed by differential testing.

- **D1** The click-window check (R17) is implied by R32/R35 plus the event window. A session start `s` is in `[ws, we)`, and a qualifying click has `s - 7d <= c.ts <= s`, which already puts it in `[ws - 7d, we)`. An unparseable or overflowing `ts` must still be rejected, not wrapped.
- **D2** A click with an empty `user_id` can never reach a person with sessions. Empty IDs never enter events (R24) or links (R18).
- **D3** Click dedup (R30) changes nothing: identical copies cannot change the latest-click choice or the set of distinct campaigns. The native version can skip it.
- **D4** The read order among events with equal `ts` affects no output.
- **D5** Every session day lies inside the window: day = floor(start / 86,400,000), and there are 14 possible values.

## 6. Ambiguities, with my defaults

None of these changes the output on generator data, as far as I can tell: the generator never produces the input in question. Each could matter on hand-made input. **Please confirm or overrule.**

1. **Final line without `\n`.** I treat it as a line and parse it. The generator always ends files with `\n`, and so do all 36 fixtures.
2. **File discovery.** Regular files directly in the folder whose name ends in `.csv` (case-sensitive) and doesn't start with `.`; no recursion. The generator only writes `part-NNNNN.csv`.
3. **Header.** Skipped unconditionally, with the documented column order assumed. The generator writes fixed headers. The native version can check the header and fall back to mapping columns by name, at almost no cost.
4. **F3 longer than 19 characters only because of leading zeros.** I parse by value, so the timestamp is valid if its value is in the window. "Too long for a 64-bit integer" could also mean too many characters. The generator never writes a leading zero, and E18's padded value is only 16 characters.
5. **Strict shapes.** The year is exactly 4 digits with no sign, and every field is exactly 2 digits. Minute 60, day 00 and month 00 are bad: the rules list hour 24 and second 60, and the others are "impossible calendar values".
6. **Unparseable `ingest_ts`** (impossible per R15). Such a copy never wins dedup; if it is the only copy, the event is dropped.
7. **Equal smallest `ingest_ts` with different contents** (impossible per R23). I keep a deterministic but arbitrary copy.
8. **Click copies that differ** (impossible per R30). I keep an arbitrary copy; see D3.
9. **`products.csv` and `campaigns.csv`.** I parse them with the same dialect (strip quotes, drop malformed lines, skip the header), and the first occurrence wins if a key repeats.
10. **Non-UTF-8 bytes** (impossible per R5). IDs are compared and written as raw bytes. DuckDB's `read_text` rejects invalid UTF-8, so only the baseline would fail on them.
11. **Output nullability and widths.** `top_category` is nullable and the rest are not; integers are `INT64`. Either choice passes the checker.
12. **The shared-device rule.** I read it as described under R21: no special case. If the track page says anything that isn't in `rules.md`, tell me.

## 7. Environment and risks

- **The scorer** runs on GCP n2 Intel Xeon, Cascade Lake or newer, with AVX-512 (starter README). So `x86-64-v3`, even `v4`, is safe there.
- **This machine** has these limits:

  | Resource | What's here                                                            |
  | -------- | ---------------------------------------------------------------------- |
  | CPU      | Pentium Gold 5405U, 2 cores / 4 threads, no AVX or AVX2                |
  | RAM      | 16 GB                                                                  |
  | Disk     | 12.6 GB free on C:, which holds Docker's disk image and probably the WSL distro |
  | Docker   | Docker Desktop is installed but not running                            |
  | WSL      | WSL2 Ubuntu is available                                               |
  | Tools    | Python 3.14 (no polars or duckdb yet), Rust 1.93                       |

  What follows from that:

  - **No AVX here.** A `v3`/`v4` binary dies with SIGILL on this machine, so I can't test what would ship. Proposal: ship `x86-64-v2` and let runtime-dispatched SIMD (which `memchr` does on its own) use AVX2 on the scorer. Only go `v3` if we can validate it somewhere with AVX2, such as GitHub Actions runners.
  - **No exact scorer run.** Docker rejects `--cpus` above the VM's 4 logical CPUs, so `--cpus 8 --memory 24g` can't run here. I'll use every scorer flag from `scripts/docker_run.sh` with `CPUS=4` and the VM's memory: `--read-only`, `--network none`, `--cap-drop ALL`, `no-new-privileges`, `--pids-limit 4096`, `--tmpfs /tmp:size=64m`, `--user 1000:1000`, and `/data` read-only. That's a superset of your example. Local times will be slower than the scorer's and are useful only for comparisons.
  - **Slow Windows mounts.** Docker bind mounts of Windows paths (`E:\…`) go through a slow file-sharing layer. For timing, the datasets should live on the WSL2 ext4 filesystem, with Docker run from the Ubuntu distro. That disk is probably on C:, so a 20M set (about 1.85 GB) fits but a 60M set (about 5.5 GB) is tight. Moving the distro to E: is possible, but I'll ask first.
  - **perf.** `perf` under WSL2 may need a custom build. If so, per-stage timers on stderr will have to do.
- **Build time.** The scorer allows 10 minutes for the build, with network. arrow-rs plus `parquet` with default features compiles slowly. A small hand-written Parquet writer (flat schema, PLAIN encoding, uncompressed, one row group per file) keeps the image build to well under a minute and gives full control over write speed. To decide in Phase 2, after checking it against polars and pyarrow.
- **Startup is timed.** The clock includes container startup, and the ~1.24 s leaders pay that too. Our own process budget is below 1.2 s.
- **Repo hygiene.** `.gitignore` excludes the zips, `dev/`, `dev-answer/`, `_ref/`, outputs, build directories and `*.parquet`. The repo is public, so the dev answer must never be committed.

## 8. Phase 1 plan (DuckDB oracle)

- **Raw input.** Read each file's raw text (`read_text`), split it on `\n` only, and drop line 1 of each file. This bypasses DuckDB's CSV reader, which reads `""` as `''` rather than null, repairs quoting, and may treat `\r` as a line end.
- **Fields.** Split each line on `,` in SQL. Check the quote rule (R9), strip the quotes, and turn `''` into NULL.
- **Timestamps.** Use regexes with `[0-9]`, never `\d`. Check the calendar explicitly (month lengths and leap years), then compute epoch milliseconds with integer arithmetic. An F3 value that overflows is bad: compare its length before casting.
- **Links.** Run union-find in Python, with path compression and union by size, over the parsed pairs. Its result is an ID-to-person table loaded back into DuckDB.
- **The rest.** Events, clicks, sessions, credit and assists are one CTE per rule, in rule order, with R-numbers in the comments. Both tables are written with `COPY … TO … (FORMAT parquet)` with explicit `DATE` / `BIGINT` / `VARCHAR` casts.
- **Tests.** All 36 fixtures first, then check-dev with zero differing rows, then the Docker run with scorer flags.
