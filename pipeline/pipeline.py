#!/usr/bin/env python3
"""
Shopping Sessions: DuckDB reference pipeline (the correctness oracle).

Implements rules.md step by step. Comments cite the rule numbers R1..R40 from NOTES.md.
Reads  $DATA_DIR    (default /data)
Writes $OUTPUT_DIR  (default /output): user_daily/ and campaign_daily/ as Parquet
Temp   $SCRATCH_DIR (default /scratch)

Design notes
- DuckDB's CSV reader repairs quoting and treats "" as '' (see starter README), so this
  pipeline never uses it. Each file is read as raw text, split on \n only (R5), and the
  dialect (R6-R9) is applied in SQL.
- Identity resolution (R18-R21) is a union-find in Python over the parsed link pairs.
"""
import json
import os
import sys
import time

import duckdb
import pyarrow as pa

DATA = os.environ.get("DATA_DIR", "/data")
OUT = os.environ.get("OUTPUT_DIR", "/output")
SCRATCH = os.environ.get("SCRATCH_DIR", "/scratch")
NCPU = int(os.environ.get("NCPU", os.cpu_count() or 1))
MEM_GB = int(os.environ.get("MEM_GB", "8"))

T0 = time.time()


def log(message):
    print(f"[{time.time() - T0:7.2f}s] {message}", file=sys.stderr, flush=True)


con = duckdb.connect()  # in-memory database
con.execute(f"SET threads = {NCPU}")
con.execute(f"SET memory_limit = '{max(1, int(MEM_GB * 0.75))}GB'")
con.execute(f"SET temp_directory = '{os.path.join(SCRATCH, 'duckdb_tmp')}'")
con.execute("SET preserve_insertion_order = false")
con.execute("SET TimeZone = 'UTC'")  # nothing below uses time zones; this is belt and braces

# ---------------------------------------------------------------------------
# Macros
# ---------------------------------------------------------------------------
con.execute(r"""
-- R9: a field is valid if it has no quote at all, or is wrapped in one pair of quotes with none inside.
CREATE MACRO field_ok(x) AS (
    NOT contains(x, '"')
    OR (length(x) >= 2 AND starts_with(x, '"') AND ends_with(x, '"')
        AND NOT contains(substring(x, 2, length(x) - 2), '"'))
);
-- R7, R8: quotes are syntax; an empty field is null.
CREATE MACRO field_val(x) AS (
    nullif(CASE WHEN starts_with(x, '"') THEN substring(x, 2, length(x) - 2) ELSE x END, '')
);
-- Floor division and modulo for possibly negative a (DuckDB's // and % truncate toward zero).
CREATE MACRO floor_mod(a, b) AS (((a % b) + b) % b);
CREATE MACRO floor_div(a, b) AS ((a - floor_mod(a, b)) // b);
-- Days in a month, Gregorian leap years (R13: Feb 29 only in leap years).
CREATE MACRO dim(y, m) AS (
    CASE WHEN m = 2 THEN CASE WHEN (y % 4 = 0 AND y % 100 <> 0) OR y % 400 = 0 THEN 29 ELSE 28 END
         WHEN m IN (4, 6, 9, 11) THEN 30
         ELSE 31 END
);
-- Days since 1970-01-01 for a valid civil date with 0 <= y <= 9999 (Howard Hinnant's
-- days_from_civil). The year is shifted by +400 so every term stays non-negative.
CREATE MACRO days_civil(y, m, d) AS (
    ((y - CASE WHEN m <= 2 THEN 1 ELSE 0 END + 400) // 400) * 146097
    + ((y - CASE WHEN m <= 2 THEN 1 ELSE 0 END + 400) % 400) * 365
    + ((y - CASE WHEN m <= 2 THEN 1 ELSE 0 END + 400) % 400) // 4
    - ((y - CASE WHEN m <= 2 THEN 1 ELSE 0 END + 400) % 400) // 100
    + (153 * ((m + 9) % 12) + 2) // 5 + d - 1
    - 719468 - 146097
);
-- R12-R14: parse a ts to epoch ms (BIGINT), or NULL if it is not one of F1, F2, F3.
-- F3 values with more than 15 significant digits are >= 1e15 ms (year 33658+), outside any
-- window an F1 manifest can express, so they are mapped to NULL (bad) instead of overflowing.
CREATE MACRO ts_ms(s) AS (
    CASE
        WHEN s IS NULL THEN NULL
        WHEN regexp_full_match(s, '[0-9]+') THEN                                     -- F3
            CASE WHEN length(ltrim(s, '0')) > 15 THEN NULL
                 ELSE CAST('0' || ltrim(s, '0') AS BIGINT) END
        WHEN regexp_full_match(s, '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(Z|[+-][0-9]{2}:[0-9]{2})') THEN
            CASE WHEN CAST(substring(s, 6, 2) AS INTEGER) BETWEEN 1 AND 12
                  AND CAST(substring(s, 9, 2) AS INTEGER) BETWEEN 1
                      AND dim(CAST(substring(s, 1, 4) AS INTEGER), CAST(substring(s, 6, 2) AS INTEGER))
                  AND CAST(substring(s, 12, 2) AS INTEGER) <= 23
                  AND CAST(substring(s, 15, 2) AS INTEGER) <= 59
                  AND CAST(substring(s, 18, 2) AS INTEGER) <= 59
                  AND (length(s) = 20                                                  -- F1: ...Z
                       OR (CAST(substring(s, 21, 2) AS INTEGER) <= 23                  -- F2 offset hours
                           AND CAST(substring(s, 24, 2) AS INTEGER) <= 59))            -- F2 offset minutes
            THEN
                CAST(days_civil(CAST(substring(s, 1, 4) AS BIGINT), CAST(substring(s, 6, 2) AS BIGINT),
                                CAST(substring(s, 9, 2) AS BIGINT)) AS BIGINT) * 86400000
                + CAST(substring(s, 12, 2) AS BIGINT) * 3600000
                + CAST(substring(s, 15, 2) AS BIGINT) * 60000
                + CAST(substring(s, 18, 2) AS BIGINT) * 1000
                - CASE WHEN length(s) = 20 THEN 0                                      -- UTC = local - offset
                       ELSE (CASE WHEN substring(s, 20, 1) = '+' THEN 1 ELSE -1 END)
                            * (CAST(substring(s, 21, 2) AS BIGINT) * 60 + CAST(substring(s, 24, 2) AS BIGINT))
                            * 60000 END
            ELSE NULL END
        ELSE NULL
    END
);
-- R24: price_cents of a purchase: digits only, value 1..10,000,000 (leading zeros allowed).
CREATE MACRO price_val(s) AS (
    CASE WHEN regexp_full_match(s, '[0-9]+') AND length(ltrim(s, '0')) <= 8
         THEN CAST('0' || ltrim(s, '0') AS BIGINT) END
);
""")


# ---------------------------------------------------------------------------
# Reading files as raw lines (R2, R3, R5)
# ---------------------------------------------------------------------------
def csv_files(folder):
    """R2: every *.csv directly in the folder (NOTES ambiguity 2)."""
    path = os.path.join(DATA, folder)
    if not os.path.isdir(path):
        return []
    return sorted(
        os.path.join(path, name) for name in os.listdir(path)
        if name.endswith(".csv") and not name.startswith(".") and os.path.isfile(os.path.join(path, name))
    )


def load_lines(table, files):
    """One row per data line: R3 drops line 1 of each file, R5 splits on \\n only.
    The empty remainder after a final \\n is not a line; a last line without \\n is (ambiguity 1)."""
    if not files:
        con.execute(f"CREATE TABLE {table} (filename VARCHAR, ln BIGINT, line VARCHAR)")
        return
    con.execute(f"""
        CREATE TABLE {table} AS
        WITH f AS (
            SELECT filename, string_split(content, chr(10)) AS ls, suffix(content, chr(10)) AS ends_nl
            FROM read_text(?)
        ),
        l AS (
            SELECT filename, len(ls) AS n, ends_nl, unnest(ls) AS line, generate_subscripts(ls, 1) AS ln
            FROM f
        )
        SELECT filename, ln, line FROM l
        WHERE ln > 1 AND NOT (ends_nl AND ln = n)
    """, [files])


def parse_fields(lines_table, n_fields, names):
    """R6-R9: split on every comma; drop lines with the wrong field count or a stray quote;
    strip the quotes; '' -> NULL."""
    cols = ", ".join(f"v[{i + 1}] AS {name}" for i, name in enumerate(names))
    return f"""
        SELECT filename, ln, {cols}
        FROM (
            SELECT filename, ln, list_transform(f, lambda x: field_val(x)) AS v
            FROM (SELECT filename, ln, string_split(line, ',') AS f FROM {lines_table})
            WHERE len(f) = {n_fields} AND list_bool_and(list_transform(f, lambda x: field_ok(x)))
        )
    """


# ---------------------------------------------------------------------------
# Manifest, products, campaigns (R1, R4)
# ---------------------------------------------------------------------------
with open(os.path.join(DATA, "manifest.json"), encoding="utf-8") as fh:
    manifest = json.load(fh)
WS, WE = con.execute("SELECT ts_ms(?), ts_ms(?)", [manifest["window_start"], manifest["window_end"]]).fetchone()
if WS is None or WE is None:
    sys.exit(f"bad manifest window: {manifest}")
CLICK_WS = WS - 604_800_000  # R17
log(f"window {manifest['window_start']} .. {manifest['window_end']} = [{WS}, {WE}) ms")

load_lines("product_lines", [os.path.join(DATA, "products.csv")])
con.execute(f"""
    CREATE TABLE products AS
    SELECT product_id, category FROM ({parse_fields('product_lines', 2, ['product_id', 'category'])})
    WHERE product_id IS NOT NULL
    QUALIFY row_number() OVER (PARTITION BY product_id ORDER BY filename, ln) = 1   -- ambiguity 9
""")
load_lines("campaign_lines", [os.path.join(DATA, "campaigns.csv")])
con.execute(f"""
    CREATE TABLE campaigns AS
    SELECT campaign_id, channel FROM ({parse_fields('campaign_lines', 2, ['campaign_id', 'channel'])})
    WHERE campaign_id IS NOT NULL
    QUALIFY row_number() OVER (PARTITION BY campaign_id ORDER BY filename, ln) = 1
""")
log(f"products {con.execute('SELECT count(*) FROM products').fetchone()[0]}, "
    f"campaigns {con.execute('SELECT count(*) FROM campaigns').fetchone()[0]}")

# ---------------------------------------------------------------------------
# Linked IDs (R18-R21)
# ---------------------------------------------------------------------------
load_lines("link_lines", csv_files("links"))
# R18: drop malformed lines and lines with an empty side.
pairs = con.execute(f"""
    SELECT a, b FROM ({parse_fields('link_lines', 2, ['a', 'b'])})
    WHERE a IS NOT NULL AND b IS NOT NULL
""").fetchall()
log(f"link pairs {len(pairs)}")

# R19: union-find (path compression + union by size); joining is transitive, repeats and
# self-links are no-ops. R20: the person's name is the byte-wise smallest of their IDs.
# (Python compares str by code point, which is the byte-wise order of their UTF-8 encoding.)
parent = {}
size = {}


def find(x):
    root = x
    while parent[root] != root:
        root = parent[root]
    while parent[x] != root:
        parent[x], x = root, parent[x]
    return root


for a, b in pairs:
    for x in (a, b):
        if x not in parent:
            parent[x] = x
            size[x] = 1
    ra, rb = find(a), find(b)
    if ra != rb:
        if size[ra] < size[rb]:
            ra, rb = rb, ra
        parent[rb] = ra
        size[ra] += size[rb]
name = {}
for x in parent:
    r = find(x)
    if r not in name or x < name[r]:
        name[r] = x
ids = list(parent)
people = [name[find(x)] for x in ids]
con.register("idmap_src", pa.table({"id": pa.array(ids, pa.string()), "person": pa.array(people, pa.string())}))
# R20: an ID that is in no remaining link line is a person on its own (COALESCE below).
con.execute("CREATE TABLE idmap AS SELECT id, person FROM idmap_src")
con.unregister("idmap_src")
del pairs, parent, size, name, ids, people
log(f"linked ids {con.execute('SELECT count(*) FROM idmap').fetchone()[0]}, "
    f"people among them {con.execute('SELECT count(DISTINCT person) FROM idmap').fetchone()[0]}")

# ---------------------------------------------------------------------------
# Events (R22-R28)
# ---------------------------------------------------------------------------
load_lines("event_lines", csv_files("events"))
log(f"event lines {con.execute('SELECT count(*) FROM event_lines').fetchone()[0]}")

# R22: parse; drop malformed lines and empty event_id.
con.execute(f"""
    CREATE TABLE ev_parsed AS
    SELECT * FROM ({parse_fields('event_lines', 7,
                    ['event_id', 'user_id', 'ts_s', 'ingest_s', 'event_type', 'product_id', 'price_s'])})
    WHERE event_id IS NOT NULL
""")
con.execute("DROP TABLE event_lines")

# R23: dedup on event_id BEFORE validation: keep the copy with the smallest ingest_ts.
# Equal ingest_ts copies are identical (R23); the other sort keys only make the pick deterministic.
# An unparseable ingest_ts (impossible per R15) sorts last (ambiguity 6).
con.execute("""
    CREATE TABLE ev_kept AS
    SELECT * FROM (SELECT *, ts_ms(ingest_s) AS ingest_ms FROM ev_parsed)
    QUALIFY row_number() OVER (
        PARTITION BY event_id
        ORDER BY ingest_ms ASC NULLS LAST, user_id, ts_s, event_type, product_id, price_s, filename, ln) = 1
""")
con.execute("DROP TABLE ev_parsed")

# R24: validate the kept copy. R25: drop late events. R28: category from products.csv.
con.execute(f"""
    CREATE TABLE ev_valid AS
    SELECT e.event_id, e.user_id, e.ts, e.event_type,
           CASE WHEN e.event_type = 'purchase' THEN e.price END AS price,
           coalesce(p.category, 'unknown') AS category
    FROM (
        SELECT *, ts_ms(ts_s) AS ts, price_val(price_s) AS price FROM ev_kept
    ) e
    LEFT JOIN products p ON p.product_id = e.product_id
    WHERE e.user_id IS NOT NULL                                     -- R24 user_id non-empty
      AND e.ts IS NOT NULL AND e.ts >= {WS} AND e.ts < {WE}         -- R24 ts valid, in event window (R16)
      AND e.event_type IN ('view', 'cart', 'purchase')              -- R24 exact type
      AND e.product_id IS NOT NULL                                  -- R24 product_id non-empty
      AND (e.event_type <> 'purchase' OR e.price BETWEEN 1 AND 10000000)   -- R24 purchase price
      AND e.ingest_ms IS NOT NULL AND e.ingest_ms - e.ts <= 86400000        -- R25 late: over 24 h is dropped
""")
con.execute("DROP TABLE ev_kept")
log(f"surviving events {con.execute('SELECT count(*) FROM ev_valid').fetchone()[0]}")

# R21: an event is its ID's person's. R26: sessionize per person, sorted by ts; a new session
# when the gap from the previous event is over 30 min. Equal ts have gap 0, so they never split;
# event_id (unique after R23) only makes the window order deterministic.
con.execute("""
    CREATE TABLE ev_sess AS
    SELECT *, sum(new_session) OVER (PARTITION BY person ORDER BY ts, event_id
                                     ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS sess_no
    FROM (
        SELECT *, CASE WHEN prev_ts IS NULL OR ts - prev_ts > 1800000 THEN 1 ELSE 0 END AS new_session
        FROM (
            SELECT e.*, coalesce(m.person, e.user_id) AS person,
                   lag(e.ts) OVER (PARTITION BY coalesce(m.person, e.user_id) ORDER BY e.ts, e.event_id) AS prev_ts
            FROM ev_valid e LEFT JOIN idmap m ON m.id = e.user_id
        )
    )
""")
con.execute("DROP TABLE ev_valid")

# R26: a session starts at its first event's ts. R27: its day is the UTC date of its start.
con.execute("""
    CREATE TABLE sessions AS
    SELECT person, sess_no, start,
           DATE '1970-01-01' + CAST(floor_div(start, 86400000) AS INTEGER) AS day,
           n_events, revenue, has_purchase
    FROM (
        SELECT person, sess_no, min(ts) AS start, count(*) AS n_events,
               CAST(coalesce(sum(price) FILTER (WHERE event_type = 'purchase'), 0) AS BIGINT) AS revenue,
               bool_or(event_type = 'purchase') AS has_purchase
        FROM ev_sess GROUP BY person, sess_no
    )
""")
log(f"sessions {con.execute('SELECT count(*) FROM sessions').fetchone()[0]}")

# ---------------------------------------------------------------------------
# Clicks (R29-R31)
# ---------------------------------------------------------------------------
load_lines("click_lines", csv_files("clicks"))
# R29: parse; drop malformed lines and empty click_id.
con.execute(f"""
    CREATE TABLE clicks_parsed AS
    SELECT * FROM ({parse_fields('click_lines', 4, ['click_id', 'user_id', 'campaign_id', 'ts_s'])})
    WHERE click_id IS NOT NULL
""")
con.execute("DROP TABLE click_lines")
# R30: dedup on click_id; copies are identical, keep any one (deterministically).
# R31: validate: user_id non-empty, ts valid and in the click window (R17), campaign known.
con.execute(f"""
    CREATE TABLE clicks_valid AS
    SELECT c.click_id, coalesce(m.person, c.user_id) AS person, c.campaign_id, c.ts
    FROM (
        SELECT *, ts_ms(ts_s) AS ts FROM clicks_parsed
        QUALIFY row_number() OVER (PARTITION BY click_id ORDER BY user_id, campaign_id, ts_s, filename, ln) = 1
    ) c
    JOIN campaigns k ON k.campaign_id = c.campaign_id
    LEFT JOIN idmap m ON m.id = c.user_id
    WHERE c.user_id IS NOT NULL
      AND c.ts IS NOT NULL AND c.ts >= {CLICK_WS} AND c.ts < {WE}
""")
con.execute("DROP TABLE clicks_parsed")
log(f"valid clicks {con.execute('SELECT count(*) FROM clicks_valid').fetchone()[0]}")

# ---------------------------------------------------------------------------
# Attribution (R32-R35)
# ---------------------------------------------------------------------------
# The qualifying clicks of each session: same person, click.ts <= start, start - click.ts <= 7 days.
con.execute("""
    CREATE TABLE session_clicks AS
    SELECT s.person, s.sess_no, c.click_id, c.campaign_id, c.ts AS click_ts
    FROM sessions s
    JOIN clicks_valid c ON c.person = s.person AND c.ts <= s.start AND s.start - c.ts <= 604800000
""")
# R32: the latest qualifying click; on a tie in ts, the byte-wise smallest click_id.
con.execute("""
    CREATE TABLE credit AS
    SELECT person, sess_no, campaign_id FROM session_clicks
    QUALIFY row_number() OVER (PARTITION BY person, sess_no ORDER BY click_ts DESC, click_id ASC) = 1
""")
# R35: every campaign with a qualifying click assists the session once.
con.execute("CREATE TABLE assists AS SELECT DISTINCT person, sess_no, campaign_id FROM session_clicks")
log(f"session-click pairs {con.execute('SELECT count(*) FROM session_clicks').fetchone()[0]}, "
    f"credited {con.execute('SELECT count(*) FROM credit').fetchone()[0]}")

# ---------------------------------------------------------------------------
# Outputs (R37-R39)
# ---------------------------------------------------------------------------
os.makedirs(os.path.join(OUT, "user_daily"), exist_ok=True)
os.makedirs(os.path.join(OUT, "campaign_daily"), exist_ok=True)

# R38: one row per person and day with at least one session.
con.execute(f"""
    COPY (
        WITH ud AS (
            SELECT person, day, count(*) AS sessions, sum(n_events) AS events, sum(revenue) AS revenue_cents
            FROM sessions GROUP BY person, day
        ),
        cat AS (
            SELECT s.person, s.day, e.category, sum(e.price) AS amount
            FROM ev_sess e JOIN sessions s ON s.person = e.person AND s.sess_no = e.sess_no
            WHERE e.event_type = 'purchase'
            GROUP BY s.person, s.day, e.category
        ),
        top AS (   -- highest summed price; a tie goes to the byte-wise smallest category
            SELECT person, day, category FROM cat
            QUALIFY row_number() OVER (PARTITION BY person, day ORDER BY amount DESC, category ASC) = 1
        )
        SELECT CAST(ud.person AS VARCHAR) AS user_id,
               CAST(ud.day AS DATE) AS day,
               CAST(ud.sessions AS BIGINT) AS sessions,
               CAST(ud.events AS BIGINT) AS events,
               CAST(ud.revenue_cents AS BIGINT) AS revenue_cents,
               CAST(top.category AS VARCHAR) AS top_category
        FROM ud LEFT JOIN top ON top.person = ud.person AND top.day = ud.day
    ) TO '{os.path.join(OUT, "user_daily", "part-0.parquet")}' (FORMAT parquet)
""")

# R39: one row per (campaign, day) with a session credited to or assisted by it.
# R33: sessions without a qualifying click are credited to organic, channel organic.
con.execute(f"""
    COPY (
        WITH credited AS (
            SELECT coalesce(c.campaign_id, 'organic') AS campaign_id, s.day,
                   count(*) AS sessions,
                   count(*) FILTER (WHERE s.has_purchase) AS purchasing_sessions,
                   sum(s.revenue) AS revenue_cents
            FROM sessions s LEFT JOIN credit c ON c.person = s.person AND c.sess_no = s.sess_no
            GROUP BY 1, 2
        ),
        assisted AS (
            SELECT a.campaign_id, s.day, count(*) AS assisted_sessions
            FROM assists a JOIN sessions s ON s.person = a.person AND s.sess_no = a.sess_no
            GROUP BY 1, 2
        ),
        joined AS (
            SELECT coalesce(c.campaign_id, a.campaign_id) AS campaign_id,
                   coalesce(c.day, a.day) AS day,
                   coalesce(c.sessions, 0) AS sessions,
                   coalesce(c.purchasing_sessions, 0) AS purchasing_sessions,
                   coalesce(c.revenue_cents, 0) AS revenue_cents,
                   coalesce(a.assisted_sessions, 0) AS assisted_sessions
            FROM credited c FULL OUTER JOIN assisted a ON a.campaign_id = c.campaign_id AND a.day = c.day
        )
        SELECT CAST(j.campaign_id AS VARCHAR) AS campaign_id,
               CAST(CASE WHEN j.campaign_id = 'organic' THEN 'organic' ELSE k.channel END AS VARCHAR) AS channel,
               CAST(j.day AS DATE) AS day,
               CAST(j.sessions AS BIGINT) AS sessions,
               CAST(j.purchasing_sessions AS BIGINT) AS purchasing_sessions,
               CAST(j.revenue_cents AS BIGINT) AS revenue_cents,
               CAST(j.assisted_sessions AS BIGINT) AS assisted_sessions
        FROM joined j LEFT JOIN campaigns k ON k.campaign_id = j.campaign_id
    ) TO '{os.path.join(OUT, "campaign_daily", "part-0.parquet")}' (FORMAT parquet)
""")

# R40 self-check, logged only (the output is what it is).
u = con.execute(f"SELECT count(*), sum(sessions), sum(revenue_cents) FROM read_parquet('{os.path.join(OUT, 'user_daily', '*.parquet')}')").fetchone()
c = con.execute(f"SELECT count(*), sum(sessions), sum(revenue_cents), sum(assisted_sessions), sum(sessions) FILTER (WHERE campaign_id <> 'organic') FROM read_parquet('{os.path.join(OUT, 'campaign_daily', '*.parquet')}')").fetchone()
log(f"user_daily rows {u[0]} sessions {u[1]} revenue {u[2]}; campaign_daily rows {c[0]} sessions {c[1]} "
    f"revenue {c[2]} assisted {c[3]} credited non-organic {c[4]}")
if (u[1] or 0) != (c[1] or 0) or (u[2] or 0) != (c[2] or 0) or (c[3] or 0) < (c[4] or 0):
    log("WARNING: R40 self-check failed")
log("done")
