#!/usr/bin/env python3
"""Backfill an orbitd database with historic runs, spans and log lines, so that the console's charts have something to show.

    backfill.py DATA_DIR/orbit-cloud.db [--hours 36] [--seed 7]

Dev only: it writes straight into the SQLite file (WAL mode, so orbitd may be running). Apps must already exist; the runs are
spread over the last --hours hours with a day/night pattern, the failing apps fail, run times are log-normal-ish in microseconds.
Run ids, trace ids and times are derived from --seed, so the same call makes the same history.
"""
import argparse, json, math, random, sqlite3, time

# app -> (runs per busy hour, failure rate, typical run time in microseconds, trigger)
PROFILES = {
    "hello": (7, 0.00, 900, "api"),
    "report": (3, 0.03, 2600, "api"),
    "etl": (4, 0.06, 18000, "api"),
    "sync-upstream": (2, 0.30, 4200, "api"),
    "ticker": (1, 0.00, 1400, "service"),
    "mailer": (9, 0.01, 1700, "queue"),
    "orders-worker": (1, 0.8, 3100, "queue"),
}
MESSAGES = {
    "hello": [("info", "hello from orbit")],
    "report": [("info", "report: collecting figures"), ("warn", "report: 2 rows had no timestamp, skipped"), ("info", "report: wrote 148 rows")],
    "etl": [("info", "etl: extracting"), ("debug", "etl: 3 batches of 500 rows"), ("info", "etl: loaded 1500 rows")],
    "sync-upstream": [("info", "sync: contacting upstream"), ("error", "sync: upstream answered 503")],
    "ticker": [("info", "ticker: up")],
    "mailer": [("info", "mailer: delivered a message")],
    "orders-worker": [("error", "orders: cannot process the message")],
}


def hex_id(rng, n):
    return "".join(rng.choice("0123456789abcdef") for _ in range(n))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("db")
    ap.add_argument("--hours", type=int, default=36)
    ap.add_argument("--seed", type=int, default=7)
    a = ap.parse_args()
    rng = random.Random(a.seed)
    db = sqlite3.connect(a.db, timeout=30)
    db.execute("PRAGMA busy_timeout = 30000")
    deployments = {app: dep for app, dep in db.execute("SELECT app, MAX(id) FROM deployments GROUP BY app")}
    now = int(time.time() * 1000)
    runs = spans = logs = 0
    pending = []   # (started, sql, params): inserted oldest first so that log ids follow time
    for app, (busy, fail, typical_us, trigger) in PROFILES.items():
        if app not in deployments:
            continue
        for h in range(a.hours, 0, -1):
            hour_start = now - h * 3_600_000
            hour_of_day = (hour_start // 3_600_000) % 24
            # busy in the middle of the (UTC) day, quiet at night, never zero for the chatty apps
            level = 0.35 + 0.65 * (0.5 + 0.5 * math.sin((hour_of_day - 6) / 24 * 2 * math.pi))
            count = max(0, int(rng.gauss(busy * level, max(1, busy * 0.25))))
            for _ in range(count):
                started = hour_start + rng.randrange(0, 3_600_000)
                failed = rng.random() < fail
                us = max(120, int(rng.lognormvariate(math.log(typical_us), 0.45)))
                if failed:
                    us = int(us * rng.uniform(0.6, 1.6))
                rid, tid = hex_id(rng, 16), hex_id(rng, 32)
                status = "failed" if failed else "succeeded"
                code = (3 if trigger == "queue" else 1) if failed else 0
                err = "exited with status %d" % code if failed else None
                peak = rng.choice([1_835_008, 1_900_544, 2_031_616, 2_097_152])
                calls = json.dumps({"log/v1": len(MESSAGES[app])})
                pending.append((started, (
                    "INSERT INTO runs (id, app, deployment, trigger, status, exit_code, error, started_at, finished_at, wall_ms,"
                    " wall_us, fuel, peak_memory, host_calls, trace_id, message_id) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    (rid, app, deployments[app], trigger, status, code, err, started, started + us // 1000 + 1, us // 1000, us,
                     None, peak, calls, tid, None))))
                runs += 1
                t0 = started * 1000
                load, inst = int(us * 0.18), int(us * 0.07)
                parts = [("run", None, 0, us + 40), ("load", "r", 5, 5 + load), ("instantiate", "r", 5 + load, 5 + load + inst),
                         ("execute", "r", 5 + load + inst, us + 30)]
                for i, (name, parent, s0, s1) in enumerate(parts):
                    attrs = {"status": status, "exit_code": code} if name == "run" else {"module": "demo"} if name == "load" else {}
                    pending.append((started, ("INSERT INTO spans (trace_id, span_id, parent_id, run_id, name, start_us, end_us, attrs) VALUES (?,?,?,?,?,?,?,?)",
                               (tid, "%s%02d" % (tid[:14], i), None if parent is None else "%s00" % tid[:14], rid, name, t0 + s0, t0 + s1, json.dumps(attrs)))))
                    spans += 1
                for k, (level_name, msg) in enumerate(MESSAGES[app]):
                    if failed and level_name not in ("error", "info"):
                        continue
                    pending.append((started + k, ("INSERT INTO logs (ts, app, run_id, level, message) VALUES (?,?,?,?,?)", (started + k, app, rid, level_name, msg))))
                    logs += 1
    for _, (sql, params) in sorted(pending, key=lambda p: p[0]):
        db.execute(sql, params)
    db.commit()
    print("backfilled %d runs, %d spans, %d log lines" % (runs, spans, logs))


main()
