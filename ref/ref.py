#!/usr/bin/env python3
"""Slow, literal reference implementation of the spec. Used only as a test oracle.

usage: ref.py DATA_DIR OUTPUT_DIR
"""
import datetime
import glob
import json
import os
import re
import sys
from collections import defaultdict

import pyarrow as pa
import pyarrow.parquet as pq

DAY = 86_400_000
GAP = 1_800_000
WEEK = 7 * DAY

F1 = re.compile(rb"^([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})Z$")
F2 = re.compile(
    rb"^([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})([+-])([0-9]{2}):([0-9]{2})$"
)
DIGITS = re.compile(rb"^[0-9]+$")


def days_from_civil(y, m, d):
    y = y - 1 if m <= 2 else y
    era = y // 400
    yoe = y - era * 400
    doy = (153 * ((m + 9) % 12) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def leap(y):
    return y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)


MDAYS = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]


def civil_ok(y, mo, d, h, mi, s):
    if not (1 <= mo <= 12):
        return False
    dim = 29 if (mo == 2 and leap(y)) else MDAYS[mo - 1]
    return 1 <= d <= dim and h <= 23 and mi <= 59 and s <= 59


def parse_ts(v):
    """Return epoch ms (python int, may be huge) or None if malformed."""
    if v is None:
        return None
    if DIGITS.match(v):
        return int(v)
    m = F1.match(v)
    if m:
        y, mo, d, h, mi, s = (int(x) for x in m.groups())
        if not civil_ok(y, mo, d, h, mi, s):
            return None
        return ((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60_000 + s * 1000
    m = F2.match(v)
    if m:
        g = m.groups()
        y, mo, d, h, mi, s = (int(x) for x in g[:6])
        sign = 1 if g[6] == b"+" else -1
        oh, om = int(g[7]), int(g[8])
        if not civil_ok(y, mo, d, h, mi, s) or oh > 23 or om > 59:
            return None
        local = ((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60_000 + s * 1000
        return local - sign * (oh * 60 + om) * 60_000
    return None


def parse_line(line, n):
    if line == b"":
        return None
    parts = line.split(b",")
    if len(parts) != n:
        return None
    out = []
    for p in parts:
        if b'"' in p:
            if len(p) >= 2 and p[0:1] == b'"' and p[-1:] == b'"' and b'"' not in p[1:-1]:
                p = p[1:-1]
            else:
                return None
        out.append(p if p != b"" else None)
    return out


def read_rows(folder, n):
    rows = []
    for path in sorted(glob.glob(os.path.join(folder, "*.csv"))):
        with open(path, "rb") as f:
            data = f.read()
        if not data:
            continue
        lines = data.split(b"\n")
        if lines and lines[-1] == b"" and data.endswith(b"\n"):
            lines.pop()
        for line in lines[1:]:
            r = parse_line(line, n)
            if r is not None:
                rows.append(r)
    return rows


def read_lookup(path):
    out = {}
    with open(path, "rb") as f:
        lines = f.read().split(b"\n")
    for line in lines[1:]:
        if line == b"":
            continue
        r = parse_line(line, 2)
        if r is None:
            continue
        out[r[0]] = r[1]
    return out


def iso_ms(s):
    v = parse_ts(s.encode())
    assert v is not None, s
    return v


def main():
    data, out = sys.argv[1], sys.argv[2]
    with open(os.path.join(data, "manifest.json")) as f:
        man = json.load(f)
    ws, we = iso_ms(man["window_start"]), iso_ms(man["window_end"])
    products = read_lookup(os.path.join(data, "products.csv"))
    campaigns = read_lookup(os.path.join(data, "campaigns.csv"))

    # ---- links ----
    parent = {}

    def find(x):
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:
            parent[x], x = root, parent[x]
        return root

    for a, b in read_rows(os.path.join(data, "links"), 2):
        if a is None or b is None:
            continue
        parent.setdefault(a, a)
        parent.setdefault(b, b)
        ra, rb = find(a), find(b)
        if ra != rb:
            parent[ra] = rb
    name = {}
    for x in parent:
        r = find(x)
        if r not in name or x < name[r]:
            name[r] = x

    def person(uid):
        if uid in parent:
            return name[find(uid)]
        return uid

    # ---- events ----
    kept = {}
    for r in read_rows(os.path.join(data, "events"), 7):
        eid = r[0]
        if eid is None:
            continue
        ing = parse_ts(r[3])
        assert ing is not None, r
        cur = kept.get(eid)
        if cur is None or ing < cur[0]:
            kept[eid] = (ing, r)
    events = defaultdict(list)  # person -> [(ts, kind, price, cat)]
    for eid, (ing, r) in kept.items():
        _, uid, ts_s, _, typ, pid, price_s = r
        if uid is None or pid is None:
            continue
        ts = parse_ts(ts_s)
        if ts is None or not (ws <= ts < we):
            continue
        if typ not in (b"view", b"cart", b"purchase"):
            continue
        price = 0
        if typ == b"purchase":
            if price_s is None or not DIGITS.match(price_s):
                continue
            price = int(price_s)
            if not (1 <= price <= 10_000_000):
                continue
        if ing - ts > DAY:
            continue
        cat = products.get(pid, b"unknown")
        events[person(uid)].append((ts, typ == b"purchase", price, cat))

    # ---- clicks ----
    seen = {}
    for r in read_rows(os.path.join(data, "clicks"), 4):
        if r[0] is None:
            continue
        seen.setdefault(r[0], r)
    clicks = defaultdict(list)  # person -> [(ts, click_id, campaign)]
    for cid, r in seen.items():
        _, uid, camp, ts_s = r
        if uid is None:
            continue
        ts = parse_ts(ts_s)
        if ts is None or not (ws - WEEK <= ts < we):
            continue
        if camp not in campaigns:
            continue
        clicks[person(uid)].append((ts, cid, camp))

    # ---- sessions, credit, assists ----
    user_daily = {}  # (person, day) -> [sessions, events, revenue, {cat: rev}]
    camp_daily = {}  # (camp, day) -> [sessions, purchasing, revenue, assisted]
    for p, evs in events.items():
        evs.sort(key=lambda e: e[0])
        sessions = []
        prev = None
        for e in evs:
            if prev is None or e[0] - prev > GAP:
                sessions.append([])
            sessions[-1].append(e)
            prev = e[0]
        pclicks = clicks.get(p, [])
        for s in sessions:
            start = s[0][0]
            day = start // DAY
            rev = sum(e[2] for e in s if e[1])
            has_p = any(e[1] for e in s)
            ud = user_daily.setdefault((p, day), [0, 0, 0, defaultdict(int)])
            ud[0] += 1
            ud[1] += len(s)
            ud[2] += rev
            for e in s:
                if e[1]:
                    ud[3][e[3]] += e[2]
            q = [c for c in pclicks if c[0] <= start and start - c[0] <= WEEK]
            if q:
                best = min(q, key=lambda c: (-c[0], c[1]))
                credit = best[2]
            else:
                credit = b"organic"
            cd = camp_daily.setdefault((credit, day), [0, 0, 0, 0])
            cd[0] += 1
            cd[1] += 1 if has_p else 0
            cd[2] += rev
            for camp in {c[2] for c in q}:
                cd = camp_daily.setdefault((camp, day), [0, 0, 0, 0])
                cd[3] += 1

    epoch = datetime.date(1970, 1, 1)
    u_rows = {k: [] for k in ["user_id", "day", "sessions", "events", "revenue_cents", "top_category"]}
    for (p, day), (ns, ne, rev, cats) in user_daily.items():
        top = None
        if cats:
            top = min(cats.items(), key=lambda kv: (-kv[1], kv[0]))[0].decode()
        u_rows["user_id"].append(p.decode())
        u_rows["day"].append(epoch + datetime.timedelta(days=day))
        u_rows["sessions"].append(ns)
        u_rows["events"].append(ne)
        u_rows["revenue_cents"].append(rev)
        u_rows["top_category"].append(top)
    c_rows = {
        k: []
        for k in [
            "campaign_id", "channel", "day", "sessions",
            "purchasing_sessions", "revenue_cents", "assisted_sessions",
        ]
    }
    for (camp, day), (ns, npur, rev, ass) in camp_daily.items():
        c_rows["campaign_id"].append(camp.decode())
        c_rows["channel"].append("organic" if camp == b"organic" else campaigns[camp].decode())
        c_rows["day"].append(epoch + datetime.timedelta(days=day))
        c_rows["sessions"].append(ns)
        c_rows["purchasing_sessions"].append(npur)
        c_rows["revenue_cents"].append(rev)
        c_rows["assisted_sessions"].append(ass)

    def table(cols):
        arrays = {}
        for k, v in cols.items():
            if k == "day":
                arrays[k] = pa.array(v, type=pa.date32())
            elif k in ("user_id", "top_category", "campaign_id", "channel"):
                arrays[k] = pa.array(v, type=pa.string())
            else:
                arrays[k] = pa.array(v, type=pa.int64())
        return pa.table(arrays)

    for sub, cols in (("user_daily", u_rows), ("campaign_daily", c_rows)):
        d = os.path.join(out, sub)
        os.makedirs(d, exist_ok=True)
        pq.write_table(table(cols), os.path.join(d, "part-0.parquet"))


if __name__ == "__main__":
    main()
