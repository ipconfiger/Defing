"""Python SDK 契约测试：get + watch。"""

import os
import threading
import time

from config_client import ConfigClient

ENDPOINTS = os.environ.get("DSH_ENDPOINTS", "http://127.0.0.1:8384").split(",")
PROJECT = os.environ.get("DSH_PROJECT", "sdk-project")


def main():
    c = ConfigClient(ENDPOINTS)
    snap = c.get(PROJECT, "dev")
    host = snap.get("groups", {}).get("redis", {}).get("host")
    print("[py] get ok: version=%s host=%s" % (snap["version"], host))
    if not host:
        raise SystemExit("[py] FAIL value mismatch: %s" % snap["groups"])

    stop = threading.Event()
    got = []

    def on_event(e):
        if e["version"] > snap["version"]:
            got.append(e["version"])

    t = threading.Thread(target=lambda: c.watch(PROJECT, "dev", on_event, stop), daemon=True)
    t.start()
    deadline = time.time() + 10
    while time.time() < deadline:
        if got:
            print("[py] watch event: v%d" % got[0])
            stop.set()
            print("[py] PASS")
            return
        time.sleep(0.1)
    stop.set()
    raise SystemExit("[py] FAIL watch timeout")


if __name__ == "__main__":
    main()
