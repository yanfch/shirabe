#!/usr/bin/env python3
import argparse
import os
import sqlite3
from datetime import datetime, time
from statistics import mean
from zoneinfo import ZoneInfo


NS_PER_SECOND = 1_000_000_000


def percentile(values, p):
    if not values:
        return None
    ordered = sorted(values)
    index = (len(ordered) - 1) * p
    lower = int(index)
    upper = min(lower + 1, len(ordered) - 1)
    weight = index - lower
    return ordered[lower] * (1 - weight) + ordered[upper] * weight


def fmt_seconds(value):
    if value is None:
        return "-"
    return f"{value:.1f}s"


def fmt_float(value):
    if value is None:
        return "-"
    return f"{value:.2f}"


def ns_from_local(dt, tz):
    return int(dt.replace(tzinfo=tz).timestamp() * NS_PER_SECOND)


def default_db_path():
    return os.path.join(os.environ.get("SHIRABE_DIR", os.path.expanduser("~/.shirabe")), "catalog.sqlite")


def parse_args():
    parser = argparse.ArgumentParser(
        description="Summarize observed LLM response delay from local Shirabe run_steps/llm_calls."
    )
    parser.add_argument("--db", default=default_db_path(), help="Path to catalog.sqlite.")
    parser.add_argument("--date", default=None, help="Local date to inspect, YYYY-MM-DD. Defaults to today.")
    parser.add_argument("--start-hour", type=int, default=6, help="Local hour to start from. Defaults to 6.")
    parser.add_argument("--end-hour", type=int, default=None, help="Exclusive local hour to end at.")
    parser.add_argument("--source", default=None, help="Optional source filter, e.g. codex, pi, claude.")
    parser.add_argument("--model", default=None, help="Optional model substring filter.")
    parser.add_argument(
        "--max-delay-seconds",
        type=float,
        default=1800,
        help="Drop larger gaps as idle/import artifacts. Defaults to 1800 seconds.",
    )
    parser.add_argument(
        "--min-tps-delay-seconds",
        type=float,
        default=1.0,
        help="Only compute output TPS when observed delay is at least this value. Defaults to 1 second.",
    )
    parser.add_argument("--slowest", type=int, default=12, help="Number of slowest calls to print.")
    return parser.parse_args()


def load_rows(conn, start_ns, end_ns, source, model):
    filters = ["l.started_at_ns >= ?", "l.started_at_ns < ?"]
    params = [start_ns, end_ns]
    if source:
        filters.append("l.source = ?")
        params.append(source)
    if model:
        filters.append("COALESCE(l.model, '') LIKE ?")
        params.append(f"%{model}%")

    sql = f"""
        SELECT
            l.llm_call_id,
            l.run_id,
            l.source,
            l.provider,
            l.model,
            l.input_tokens,
            l.output_tokens,
            l.started_at_ns,
            l.observed_trigger_step_id,
            l.observed_trigger_step_type,
            COALESCE(trigger.name, l.observed_latency_quality, '-'),
            l.observed_response_delay_ns,
            l.observed_output_tps,
            l.observed_latency_quality
        FROM llm_calls l
        LEFT JOIN run_steps trigger ON trigger.step_id = l.observed_trigger_step_id
        WHERE {" AND ".join(filters)}
          AND l.observed_response_delay_ns IS NOT NULL
        ORDER BY l.started_at_ns ASC
    """
    return conn.execute(sql, params).fetchall()


def summarize(rows, tz, max_delay_seconds, min_tps_delay_seconds):
    kept = []
    missing_trigger = 0
    invalid_delay = 0
    dropped_slow = 0

    for row in rows:
        (
            llm_call_id,
            run_id,
            source,
            provider,
            model,
            input_tokens,
            output_tokens,
            started_at_ns,
            trigger_step_id,
            trigger_step_type,
            trigger_name,
            observed_response_delay_ns,
            observed_output_tps,
            observed_latency_quality,
        ) = row
        if observed_response_delay_ns is None:
            missing_trigger += 1
            continue
        delay_ns = observed_response_delay_ns
        if delay_ns < 0:
            invalid_delay += 1
            continue
        delay_seconds = delay_ns / NS_PER_SECOND
        if max_delay_seconds is not None and delay_seconds > max_delay_seconds:
            dropped_slow += 1
            continue
        output_tps = observed_output_tps
        started = datetime.fromtimestamp(started_at_ns / NS_PER_SECOND, tz)
        kept.append(
            {
                "llm_call_id": llm_call_id,
                "run_id": run_id,
                "source": source or "-",
                "provider": provider or "-",
                "model": model or "-",
                "input_tokens": input_tokens or 0,
                "output_tokens": output_tokens or 0,
                "started": started,
                "hour": started.strftime("%H:00"),
                "delay_seconds": delay_seconds,
                "output_tps": output_tps,
                "trigger_step_type": trigger_step_type or "-",
                "trigger_name": trigger_name or "-",
                "quality": observed_latency_quality or "-",
            }
        )

    return kept, {
        "missing_trigger": missing_trigger,
        "invalid_delay": invalid_delay,
        "dropped_slow": dropped_slow,
    }


def print_grouped(title, rows, key_fn):
    print(f"\n{title}")
    print("bucket                 calls  sub1s  p50_delay  p90_delay  avg_tps  p50_tps  tps_n  out_tok")
    print("--------------------  -----  -----  ---------  ---------  -------  -------  -----  -------")
    groups = {}
    for row in rows:
        groups.setdefault(key_fn(row), []).append(row)

    for key in sorted(groups):
        bucket = groups[key]
        delays = [row["delay_seconds"] for row in bucket]
        tps_values = [row["output_tps"] for row in bucket if row["output_tps"] is not None]
        out_tokens = sum(row["output_tokens"] for row in bucket)
        subsecond_count = sum(1 for row in bucket if row["delay_seconds"] < 1.0)
        avg_tps = mean(tps_values) if tps_values else None
        p50_tps = percentile(tps_values, 0.5) if tps_values else None
        print(
            f"{key[:20]:20}  {len(bucket):5d}  "
            f"{subsecond_count:5d}  "
            f"{fmt_seconds(percentile(delays, 0.5)):>9}  "
            f"{fmt_seconds(percentile(delays, 0.9)):>9}  "
            f"{fmt_float(avg_tps):>7}  "
            f"{fmt_float(p50_tps):>7}  "
            f"{len(tps_values):5d}  "
            f"{out_tokens:7d}"
        )


def print_slowest(rows, count):
    if count <= 0:
        return
    print(f"\nSlowest {count} calls")
    print("time      delay   tps     source  provider  model                 out  trigger")
    print("--------  ------  ------  ------  --------  --------------------  ---  -------")
    for row in sorted(rows, key=lambda item: item["delay_seconds"], reverse=True)[:count]:
        print(
            f"{row['started'].strftime('%H:%M:%S')}  "
            f"{fmt_seconds(row['delay_seconds']):>6}  "
            f"{fmt_float(row['output_tps']):>6}  "
            f"{row['source'][:6]:6}  "
            f"{row['provider'][:8]:8}  "
            f"{row['model'][:20]:20}  "
            f"{row['output_tokens']:3d}  "
            f"{row['trigger_step_type']}:{row['trigger_name'][:20]}"
        )


def main():
    args = parse_args()
    tz = ZoneInfo(os.environ.get("TZ", "Asia/Shanghai"))
    day = datetime.strptime(args.date, "%Y-%m-%d").date() if args.date else datetime.now(tz).date()
    start = datetime.combine(day, time(args.start_hour, 0))
    end_hour = args.end_hour if args.end_hour is not None else 24
    end = datetime.combine(day, time(end_hour, 0)) if end_hour < 24 else datetime.combine(day, time(23, 59, 59, 999999))
    now = datetime.now(tz).replace(tzinfo=None)
    if day == now.date() and end > now:
        end = now

    conn = sqlite3.connect(args.db)
    rows = load_rows(conn, ns_from_local(start, tz), ns_from_local(end, tz), args.source, args.model)
    kept, skipped = summarize(rows, tz, args.max_delay_seconds, args.min_tps_delay_seconds)

    print(f"DB: {args.db}")
    print(f"Range: {start} -> {end} local")
    print(f"Rows: {len(rows)} llm_calls, {len(kept)} with observed delay")
    print(
        "Skipped: "
        f"{skipped['missing_trigger']} missing trigger, "
        f"{skipped['invalid_delay']} invalid delay, "
        f"{skipped['dropped_slow']} over max delay"
    )

    if not kept:
        return

    print_grouped("By hour", kept, lambda row: row["hour"])
    print_grouped("By hour/source/provider/model", kept, lambda row: f"{row['hour']} {row['source']} {row['provider']} {row['model']}")
    print_slowest(kept, args.slowest)


if __name__ == "__main__":
    main()
