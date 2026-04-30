#!/usr/bin/env python3
"""Extract ghost fill transaction hashes from GhostGuard verdict logs.

Usage:
    python3 scripts/extract_ghosts.py                    # all ghosts
    python3 scripts/extract_ghosts.py --reason timeout   # only timeouts
    python3 scripts/extract_ghosts.py --csv               # CSV format
    python3 scripts/extract_ghosts.py --json              # full JSON
"""

import argparse
import json
import sys
from pathlib import Path
from datetime import datetime, timezone

DEFAULT_LOG = "logs/verdicts.jsonl"

def main():
    parser = argparse.ArgumentParser(description="Extract ghost fill tx hashes")
    parser.add_argument("--log", default=DEFAULT_LOG, help="Path to verdicts.jsonl")
    parser.add_argument("--reason", help="Filter by revert reason (e.g. timeout, SafeMath, TRANSFER_FROM_FAILED)")
    parser.add_argument("--csv", action="store_true", help="Output CSV format")
    parser.add_argument("--json", action="store_true", help="Output full JSON records")
    parser.add_argument("--since", help="Only show ghosts after this UTC timestamp (YYYY-MM-DD or YYYY-MM-DDTHH:MM)")
    parser.add_argument("--summary", action="store_true", help="Print summary stats only")
    args = parser.parse_args()

    log_path = Path(args.log)
    if not log_path.exists():
        # fallback to data/ if logs/ doesn't exist yet
        log_path = Path("data/verdicts.jsonl")
        if not log_path.exists():
            print(f"Error: no verdict log found at {args.log} or data/verdicts.jsonl", file=sys.stderr)
            sys.exit(1)

    ghosts = []
    since_ts = 0
    if args.since:
        try:
            dt = datetime.fromisoformat(args.since).replace(tzinfo=timezone.utc)
            since_ts = int(dt.timestamp())
        except ValueError:
            print(f"Error: invalid date format '{args.since}'. Use YYYY-MM-DD or YYYY-MM-DDTHH:MM", file=sys.stderr)
            sys.exit(1)

    with open(log_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue

            if obj.get("verdict") not in ("Ghost", "Timeout"):
                continue

            if since_ts and obj.get("ts", 0) < since_ts:
                continue

            if args.reason:
                r = args.reason.lower()
                if r == "timeout" and obj["verdict"] != "Timeout":
                    continue
                elif r != "timeout":
                    reason = obj.get("reason", "").lower()
                    if r not in reason:
                        continue

            ghosts.append(obj)

    if args.summary:
        from collections import Counter
        print(f"Total ghost fills: {len(ghosts)}")
        by_verdict = Counter(g["verdict"] for g in ghosts)
        for v, c in by_verdict.most_common():
            print(f"  {v}: {c}")
        total_usd = sum(g.get("size", 0) * g.get("price", 0) for g in ghosts)
        print(f"Total USDC exposure: ${total_usd:,.2f}")
        reasons = Counter(g.get("reason", "Timeout") for g in ghosts)
        print("Revert reasons:")
        for r, c in reasons.most_common():
            print(f"  {r}: {c}")
        return

    if args.json:
        for g in ghosts:
            print(json.dumps(g))
    elif args.csv:
        print("tx_hash,verdict,market,price,size,usdc,reason,timestamp")
        for g in ghosts:
            reason = g.get("reason", "Timeout") if g["verdict"] == "Ghost" else "Timeout"
            ts = datetime.fromtimestamp(g.get("ts", 0), tz=timezone.utc).strftime("%Y-%m-%d %H:%M:%S")
            usdc = g.get("size", 0) * g.get("price", 0)
            print(f'{g["tx"]},{g["verdict"]},{g.get("market","")},{g.get("price",0)},{g.get("size",0)},{usdc:.2f},{reason},{ts}')
    else:
        # Plain tx hash list (one per line) — easy to pipe to other tools
        for g in ghosts:
            print(g["tx"])

    print(f"\n# {len(ghosts)} ghost fills extracted from {log_path}", file=sys.stderr)

if __name__ == "__main__":
    main()
