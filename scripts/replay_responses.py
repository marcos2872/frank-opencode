#!/usr/bin/env python3
"""Replay a dumped Responses body against the zen backend and bisect the 400.

Usage:
  python3 replay.py <dump.json> full          # replay body verbatim
  python3 replay.py <dump.json> ablate <k>    # remove top-level key k
  python3 replay.py <dump.json> front <n>     # drop first n input items (orphan outputs filtered)
  python3 replay.py <dump.json> tailkeep <n>  # keep last n input items

Exit code 0 = upstream accepted (2xx), 1 = rejected, 2 = setup error.
Accepting requests costs input tokens; the caller aborts the stream early.
"""
import json
import sqlite3
import sys
import time
import urllib.error
import urllib.request

DB = "file:/home/marcos/.local/share/opencode/opencode.db?mode=ro"
URL = "https://opencode.ai/zen/go/v1/responses"
INTEGRATION = "opencode-go"


def bearer() -> str:
    conn = sqlite3.connect(DB, uri=True)
    row = conn.execute(
        "SELECT value FROM credential WHERE integration_id = ? "
        "AND (active = 1 OR active IS NULL) LIMIT 1",
        (INTEGRATION,),
    ).fetchone()
    if not row:
        raise SystemExit("no credential for " + INTEGRATION)
    raw = row[0]
    try:
        v = json.loads(raw)
        if isinstance(v, dict):
            for f in ("access", "key"):
                k = v.get(f)
                if isinstance(k, str) and k:
                    return k
    except json.JSONDecodeError:
        pass
    return raw


def send(body: dict, token: str) -> tuple[int, str]:
    data = json.dumps(body).encode()
    req = urllib.request.Request(
        URL,
        data=data,
        headers={
            "content-type": "application/json",
            "authorization": "Bearer " + token,
            "x-api-key": token,
            # Cloudflare rejects urllib's default UA with 403/1010; the
            # gateway sends this same one from reqwest.
            "user-agent": "frank-opencode/0.2.2",
            # Required by the Go backend for routing (gateway always sends it).
            "x-opencode-session": "replay-diag-0001",
        },
    )
    for attempt in range(3):
        try:
            resp = urllib.request.urlopen(req, timeout=90)
            # Accepted: drain a little to confirm the stream opens, then abort
            # so generation stops early (input still billed).
            resp.read(1024)
            resp.close()
            return resp.status, "accepted (stream aborted after 1KB)"
        except urllib.error.HTTPError as e:
            err = e.read().decode(errors="replace")[:800]
            if e.code == 429 and attempt < 2:
                time.sleep(30)
                continue
            return e.code, err
        except Exception as e:  # noqa: BLE001
            return 0, f"transport: {e}"
    return 0, "retries exhausted"


def drop_front(body: dict, n: int) -> dict:
    items = body.get("input", [])
    kept = items[n:]
    calls = {
        i.get("call_id")
        for i in kept
        if isinstance(i, dict) and i.get("type") == "function_call"
    }
    kept = [
        i
        for i in kept
        if not (isinstance(i, dict) and i.get("type") == "function_call_output"
                and i.get("call_id") not in calls)
    ]
    out = dict(body)
    out["input"] = kept
    return out


def keep_tail(body: dict, n: int) -> dict:
    items = body.get("input", [])
    tail = items[len(items) - n:] if n < len(items) else list(items)
    # Prefix must start with a coherent segment: filter orphan outputs.
    calls = {
        i.get("call_id")
        for i in tail
        if isinstance(i, dict) and i.get("type") == "function_call"
    }
    tail = [
        i
        for i in tail
        if not (isinstance(i, dict) and i.get("type") == "function_call_output"
                and i.get("call_id") not in calls)
    ]
    out = dict(body)
    out["input"] = tail
    return out


def main() -> None:
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    path, mode = sys.argv[1], sys.argv[2]
    body = json.load(open(path))
    tok = bearer()

    if mode == "full":
        target = body
    elif mode == "ablate" and len(sys.argv) > 3:
        target = {k: v for k, v in body.items() if k != sys.argv[3]}
    elif mode == "front" and len(sys.argv) > 3:
        target = drop_front(body, int(sys.argv[3]))
    elif mode == "tailkeep" and len(sys.argv) > 3:
        target = keep_tail(body, int(sys.argv[3]))
    elif mode == "pick" and len(sys.argv) > 3:
        idx = [int(x) for x in sys.argv[3].split(",")]
        out = dict(body)
        out["input"] = [body["input"][i] for i in idx]
        target = out
    else:
        raise SystemExit(__doc__)

    n_items = len(target.get("input", []))
    wire = len(json.dumps(target).encode())
    print(f"mode={mode} args={' '.join(sys.argv[3:])} items={n_items} wire={wire}", flush=True)
    time.sleep(1)
    status, detail = send(target, tok)
    print(f"HTTP {status}: {detail}", flush=True)
    sys.exit(0 if 200 <= status < 300 else 1)


if __name__ == "__main__":
    main()
