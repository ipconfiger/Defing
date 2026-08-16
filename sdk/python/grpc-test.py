"""Python SDK gRPC 契约测试：get / get_item / watch / list_members（:8383）。"""

import os
import sys
import threading
import time

import grpc

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from config_client import ConfigClient

GRPC = os.environ.get("DSH_GRPC", "127.0.0.1:8383")
HTTP = os.environ.get("DSH_HTTP", "http://127.0.0.1:8384")
PROJECT = os.environ.get("DSH_PROJECT", "sdk-project")


def main():
    c = ConfigClient([{"grpc": GRPC, "http": HTTP}])
    snap = c.get(PROJECT, "dev")
    host = snap.get("groups", {}).get("redis", {}).get("host")
    print("[py-grpc] get ok: version=%s host=%s" % (snap["version"], host))
    if not host:
        raise SystemExit("[py-grpc] FAIL value mismatch")

    item = c.get_item(PROJECT, "dev", "redis", "host")
    print("[py-grpc] get_item ok: %s" % item)
    if item != host:
        raise SystemExit("[py-grpc] FAIL get_item mismatch")

    # D-TEST：ListMembers 真断言——dev-single 下应为 FailedPrecondition（gRPC code 9）
    try:
        members = c.list_members()
        if members:
            raise SystemExit("[py-grpc] FAIL list_members 不应在 dev-single 返回成员: %r" % members)
        print("[py-grpc] list_members dev-single 返回空列表（契约语义：非集群可用）")
    except Exception as e:
        # grpc.RpcError：code() 方法返回 StatusCode 枚举（跨版本 .value 形状不一，直接枚举比较）
        code_fn = getattr(e, "code", None)
        if callable(code_fn) and code_fn() == grpc.StatusCode.FAILED_PRECONDITION:
            print("[py-grpc] list_members dev-single → FailedPrecondition ✅")
        else:
            raise SystemExit("[py-grpc] FAIL list_members unexpected: %r" % e)

    stop = threading.Event()
    got = []

    def on_event(e):
        if e["version"] > snap["version"]:
            got.append(e["version"])

    t = threading.Thread(target=lambda: c.watch(PROJECT, "dev", on_event, stop), daemon=True)
    t.start()
    deadline = time.time() + 15
    while time.time() < deadline:
        if got:
            print("[py-grpc] watch event: v%d" % got[0])
            stop.set()
            print("[py-grpc] PASS")
            return
        time.sleep(0.1)
    stop.set()
    raise SystemExit("[py-grpc] FAIL watch timeout")


if __name__ == "__main__":
    main()
