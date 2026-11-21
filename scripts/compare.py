#!/usr/bin/env python3
"""Compare two answers. Each side is an output dir holding user_daily/ and
campaign_daily/ Parquet folders, or a dir holding user_daily.csv and
campaign_daily.csv (an expected-answer dir). Exit 1 on any difference.

usage: compare.py EXPECTED ACTUAL
"""
import os
import sys

import duckdb

TABLES = {
    "user_daily": ["user_id", "day", "sessions", "events", "revenue_cents", "top_category"],
    "campaign_daily": [
        "campaign_id", "channel", "day", "sessions",
        "purchasing_sessions", "revenue_cents", "assisted_sessions",
    ],
}


def source(root, table):
    csv = os.path.join(root, f"{table}.csv")
    if os.path.exists(csv):
        return f"read_csv('{csv}', header=true, all_varchar=true)"
    files = [
        f for f in os.listdir(os.path.join(root, table))
        if f.endswith(".parquet") and not f.startswith((".", "_"))
    ]
    if not files:
        return None
    return f"read_parquet('{os.path.join(root, table)}/[!._]*.parquet')"


def normalized(con, src, cols):
    exprs = []
    for c in cols:
        if c == "day":
            exprs.append(f"CAST(CAST({c} AS DATE) AS VARCHAR) AS {c}")
        elif c == "top_category":
            exprs.append(f"NULLIF(CAST({c} AS VARCHAR), '') AS {c}")
        else:
            exprs.append(f"CAST({c} AS VARCHAR) AS {c}")
    return f"SELECT {', '.join(exprs)} FROM {src}"


def main():
    exp, act = sys.argv[1], sys.argv[2]
    con = duckdb.connect()
    bad = False
    for table, cols in TABLES.items():
        a, b = source(exp, table), source(act, table)
        if b is not None:
            actual_cols = [r[0] for r in con.execute(f"DESCRIBE SELECT * FROM {b}").fetchall()]
            if sorted(actual_cols) != sorted(cols):
                print(f"{table}: wrong columns {actual_cols}")
                bad = True
                continue
            if "parquet" in b:
                types = dict(con.execute(f"DESCRIBE SELECT * FROM {b}").fetchall()[i][:2]
                             for i in range(len(cols)))
                if types["day"] != "DATE":
                    print(f"{table}: day is {types['day']}, not DATE")
                    bad = True
        na = normalized(con, a, cols) if a else None
        nb = normalized(con, b, cols) if b else None
        ea = con.execute(f"SELECT count(*) FROM ({na})").fetchone()[0] if na else 0
        eb = con.execute(f"SELECT count(*) FROM ({nb})").fetchone()[0] if nb else 0
        if na and nb:
            missing = con.execute(f"SELECT * FROM ({na}) EXCEPT ALL SELECT * FROM ({nb})").fetchall()
            extra = con.execute(f"SELECT * FROM ({nb}) EXCEPT ALL SELECT * FROM ({na})").fetchall()
        else:
            missing, extra = ([] if not na else ["<all>"]), ([] if not nb else ["<all>"])
        status = "OK" if not missing and not extra else "DIFF"
        print(f"{table}: expected {ea} rows, actual {eb} rows, missing {len(missing)}, extra {len(extra)} -> {status}")
        for r in missing[:5]:
            print("  missing:", r)
        for r in extra[:5]:
            print("  extra:  ", r)
        bad |= status != "OK"
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
