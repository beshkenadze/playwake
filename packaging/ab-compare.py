#!/usr/bin/env python3
"""A/B journal comparison: Python worker vs native playwake-media worker.

Usage: ab-compare.py JOURNAL_PY JOURNAL_RS [--height 1080]

Aligns per-resolution segments by `stream_requested`, then compares the
latency-critical metrics the host journals (src/media.rs). Exit code 1 on any
failed threshold. Stdlib only.
"""
import json
import statistics
import sys


def load(path):
    segments = {}
    current = None
    errors = []
    with open(path, errors="ignore") as handle:
        for line in handle:
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                record = json.loads(line)
            except ValueError:
                continue
            event = record.get("event")
            details = record.get("details") or {}
            if event == "stream_requested":
                current = details.get("height")
                segments.setdefault(current, {"video_active": [], "hud": [], "latency_video": [],
                                              "video_stats": [], "errors": []})
            if current is None:
                continue
            bucket = segments[current]
            if event in ("gstreamer_error", "first_video_timeout", "probe_unavailable"):
                bucket["errors"].append(event)
                errors.append(event)
            elif event == "video_active":
                bucket["video_active"].append(details)
            elif event == "hud-stats":
                bucket["hud"].append(details)
            elif event == "stream_diagnostics_sample":
                bucket["hud"].append(details)
            elif event == "latency_video":
                bucket["latency_video"].append(details)
            elif event == "video_stats":
                bucket["video_stats"].append(details)
    return segments, errors


def median(values):
    values = [v for v in values if isinstance(v, (int, float))]
    return statistics.median(values) if values else None


def stats_fps(bucket):
    points = [(v.get("worker_time_ms"), v.get("decoded_frames")) for v in bucket["video_stats"]
              if v.get("worker_time_ms") and v.get("decoded_frames") is not None]
    rates = []
    for (t0, d0), (t1, d1) in zip(points, points[1:]):
        if t1 > t0 and d1 >= d0:
            rates.append((d1 - d0) * 1000.0 / (t1 - t0))
    return median(rates)


def summarize(bucket):
    active = bucket["video_active"][-1] if bucket["video_active"] else {}
    fps = median([h.get("decodedFps", h.get("decoded_fps")) for h in bucket["hud"]]) or stats_fps(bucket)
    recv = median([l.get("receive_to_sink_ms") for l in bucket["latency_video"]])
    drops = max([h.get("queueDrops", h.get("estimated_queue_drops")) or 0 for h in bucket["hud"]] or [0])
    sink_drops = max([h.get("sinkDrops", h.get("sink_drops")) or 0 for h in bucket["hud"]] or [0])
    lost = max([h.get("rtpLost", h.get("rtp_lost")) or 0 for h in bucket["hud"]] or [0])
    return {
        "decoder": active.get("decoder"), "renderer": active.get("renderer"),
        "conversion": active.get("conversion"),
        "fps": fps, "recv_ms": recv, "queue_drops": drops, "sink_drops": sink_drops,
        "rtp_lost": lost, "errors": bucket["errors"],
    }


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    height_filter = None
    if "--height" in sys.argv:
        height_filter = int(sys.argv[sys.argv.index("--height") + 1])
    if len(args) != 2:
        print(__doc__)
        return 2
    py_segments, _ = load(args[0])
    rs_segments, _ = load(args[1])
    failures = []
    for height in sorted(set(py_segments) & set(rs_segments)):
        if height_filter and height != height_filter:
            continue
        py = summarize(py_segments[height])
        rs = summarize(rs_segments[height])
        print(f"== {height}p")
        for key in ("decoder", "renderer", "conversion", "fps", "recv_ms",
                    "queue_drops", "sink_drops", "rtp_lost", "errors"):
            print(f"  {key:12} py={py[key]!r:28} rs={rs[key]!r}")
        if (py["decoder"], py["renderer"], py["conversion"]) != (rs["decoder"], rs["renderer"], rs["conversion"]):
            failures.append(f"{height}p: video_active tuple differs")
        if py["fps"] and rs["fps"]:
            if abs(rs["fps"] - py["fps"]) > 0.02 * py["fps"]:
                failures.append(f"{height}p: decoded fps {rs['fps']} vs {py['fps']} (>2%)")
        else:
            failures.append(f"{height}p: missing fps samples")
        if py["recv_ms"] is not None and rs["recv_ms"] is not None:
            tolerance = max(1.5, 0.10 * py["recv_ms"])
            if rs["recv_ms"] - py["recv_ms"] > tolerance:
                failures.append(f"{height}p: receive_to_sink {rs['recv_ms']} vs {py['recv_ms']} (tol {tolerance:.2f})")
        if rs["errors"]:
            failures.append(f"{height}p: native errors {rs['errors']}")
        if rs["sink_drops"] > py["sink_drops"] or rs["queue_drops"] > py["queue_drops"] + 2:
            failures.append(f"{height}p: native drops worse")
    missing = set(py_segments) - set(rs_segments)
    if missing:
        failures.append(f"missing native segments: {sorted(missing)}")
    if failures:
        print("FAIL:")
        for failure in failures:
            print(" -", failure)
        return 1
    print("PASS: native worker within A/B thresholds")
    return 0


if __name__ == "__main__":
    sys.exit(main())