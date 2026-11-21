# sablemoth

A fast, exact batch pipeline in Rust. It reads dirty CSV event, click and
identity-link logs, merges each person's IDs, splits their activity into
sessions, attributes every session to an ad campaign, and writes two daily
Parquet tables.

## Quick start (Docker)

```bash
docker build --platform linux/amd64 -t sablemoth .

docker run --rm --platform linux/amd64 \
  --cpus 8 --memory 24g --memory-swap 24g \
  --read-only --tmpfs /tmp:size=64m -u 1000 --network none \
  -e DATA_DIR=/data -e OUTPUT_DIR=/output -e NCPU=8 \
  -v "$PWD/data:/data:ro" -v "$PWD/out:/output" \
  sablemoth
```

The image runs as uid 1000, needs no network at run time, and writes only
under `OUTPUT_DIR`. Logs (stage timings) go to stderr; nothing is printed to
stdout.

## Build and run without Docker

```bash
cargo build --release
DATA_DIR=path/to/data OUTPUT_DIR=path/to/out NCPU=8 ./target/release/sessions
```

| Variable | Default | Meaning |
|---|---|---|
| `DATA_DIR` | `/data` | input directory (read-only is fine) |
| `OUTPUT_DIR` | `/output` | output directory; `user_daily/` and `campaign_daily/` are created inside it |
| `NCPU` | `8` | worker threads |

## Input

```
DATA_DIR/
  manifest.json      {"window_start": "...Z", "window_end": "...Z"}
  products.csv       product_id,category
  campaigns.csv      campaign_id,channel
  events/*.csv       event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents
  clicks/*.csv       click_id,user_id,campaign_id,ts
  links/*.csv        user_id_a,user_id_b
```

Every CSV in `events/`, `clicks/` and `links/` is read, whatever the count and
size, including header-only and empty files. Malformed lines, bad timestamps
(three accepted forms: `YYYY-MM-DDTHH:MM:SSZ`, `...±HH:MM`, epoch
milliseconds), duplicates and late rows are handled as described in the
specification the pipeline was written against.

## Output

`OUTPUT_DIR/user_daily/*.parquet`: one row per person and day with a session.

| Column | Type |
|---|---|
| `user_id` | string (the person's byte-wise smallest ID) |
| `day` | DATE |
| `sessions`, `events`, `revenue_cents` | int64 |
| `top_category` | string, nullable |

`OUTPUT_DIR/campaign_daily/*.parquet`: one row per campaign (or `organic`) and
day with a credited or assisted session.

| Column | Type |
|---|---|
| `campaign_id`, `channel` | string |
| `day` | DATE |
| `sessions`, `purchasing_sessions`, `revenue_cents`, `assisted_sessions` | int64 |

## How it works

1. **Scan**: every input file is memory-mapped and split into 4 MB chunks
   processed in parallel. A scanner builds 64-byte SSE2 bitmasks of `,` `"`
   `\n` and emits fields straight from the mask bits, enforcing the quoting
   and field-count rules.
2. **IDs**: each ID becomes an exact `u64`. IDs of up to 9 ASCII bytes are
   packed as 7-bit codes, so integer order equals byte order; anything else is
   hashed, with its bytes kept in a side table for exact comparison.
3. **People**: link pairs are merged with union-find on a side thread while
   events parse; each group is named by its smallest ID.
4. **Dedup**: events are bucketed by event ID; each bucket keeps the earliest
   ingested copy, then drops it if invalid.
5. **Sessions and attribution**: survivors are bucketed by person, sorted,
   split into sessions, credited to the latest qualifying click, and counted
   for every assisting campaign.
6. **Write**: both tables are written as uncompressed Parquet in parallel,
   then the process exits without tearing down its memory.

Performance on 8 hyperthreads: about 1.1 s for 20M events and 3.3 s for 60M
events, peak memory well under 24 GB.

## Testing

`ref/ref.py` is a slow, literal Python implementation used as a test oracle;
`scripts/compare.py` diffs two outputs (Parquet or CSV) with DuckDB.

```bash
cargo test --release                                  # unit tests
python ref/ref.py tests/fixtures/edge out/ref         # oracle on a fixture
DATA_DIR=tests/fixtures/edge OUTPUT_DIR=out/rust ./target/release/sessions
python scripts/compare.py out/ref out/rust            # exits 1 on any difference
```

`scripts/fuzz.sh FIRST LAST [EVENTS]` runs the binary and the oracle on many
generated datasets and compares them; `scripts/bench.sh DATA [RUNS]` times
pinned runs. Both expect the dataset generator and Python environment in
local, untracked directories.
