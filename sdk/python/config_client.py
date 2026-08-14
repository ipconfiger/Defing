"""Defing ConfigClient — Python SDK。

双通道数据面：
- 端点传纯字符串 → HTTP/SSE（urllib 标准库，无外部依赖，降级通道）
- 端点传 {"grpc": "host:8383", "http": "..."} → gRPC 优先（需要 grpcio；懒加载）
两通道 API 形状一致；secret 值数据面脱敏为 "***"。
"""

import json
import time
import urllib.error
import urllib.request

BACKOFF_BASE_MS = 200


class ConfigError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


def _value_from_proto(v) -> object:
    if v is None:
        return None
    t = v.WhichOneof("data")
    if t == "str_value":
        return v.str_value
    if t == "int_value":
        return v.int_value
    if t == "float_value":
        return v.float_value
    if t == "bool_value":
        return v.bool_value
    if t == "json_value":
        return v.json_value
    if t == "list_value":
        return list(v.list_value.values)
    # secret 等：脱敏
    return "***"


def _snapshot_from_proto(s) -> dict:
    groups = {}
    for g, gd in s.groups.items():
        groups[g] = {k: _value_from_proto(v) for k, v in gd.items.items()}
    return {
        "project": s.project,
        "branch": s.branch,
        "version": s.version,
        "structure_version": s.structure_version,
        "groups": groups,
    }


class ConfigClient:
    def __init__(self, endpoints, *, tls: bool = False, token=None):
        self.endpoints = endpoints
        self.token = token
        self._grpc_stub = None
        self._grpc_channel = None
        first = endpoints[0]
        if isinstance(first, dict) and first.get("grpc"):
            import grpc  # 懒加载：无 grpcio 环境仍可用 HTTP 通道

            from config import v1_pb2_grpc

            self._grpc_channel = grpc.insecure_channel(first["grpc"])
            self._grpc_stub = v1_pb2_grpc.ConfigServiceStub(self._grpc_channel)
            self._meta = [("authorization", "Bearer " + token)] if token else None

    # ---------------- HTTP/SSE 通道（urllib 标准库） ----------------

    def _request(self, path):
        last = None
        for i, ep in enumerate(self.endpoints):
            url = ep if isinstance(ep, str) else ep.get("http")
            if not url:
                continue
            try:
                headers = {}
                if self.token:
                    headers["Authorization"] = "Bearer " + self.token
                req = urllib.request.Request(url + path, headers=headers)
                with urllib.request.urlopen(req, timeout=5) as r:
                    return json.loads(r.read().decode())
            except (urllib.error.URLError, OSError) as e:
                last = e
                time.sleep((i + 1) * BACKOFF_BASE_MS / 1000)
        raise ConfigError("NO_ENDPOINT", "all endpoints unreachable: %s" % last)

    # ---------------- gRPC 通道 ----------------

    def _grpc(self):
        if self._grpc_stub is None:
            raise ConfigError("NO_GRPC", "需要 gRPC 端点（{'grpc': ...}）")
        return self._grpc_stub

    def _grpc_get(self, project, branch, version):
        from config import v1_pb2

        resp = self._grpc().GetConfig(
            v1_pb2.GetConfigRequest(project=project, branch=branch, version=version),
            metadata=self._meta,
        )
        return _snapshot_from_proto(resp)

    # ---------------- 公共 API（双通道一致） ----------------

    def get(self, project, branch, version: int = 0):
        if self._grpc_stub is not None:
            return self._grpc_get(project, branch, version)
        return self._request("/v1/projects/%s/branches/%s/snapshot" % (project, branch))

    def get_item(self, project, branch, group, key, version: int = 0):
        if self._grpc_stub is not None:
            return self._grpc_get(project, branch, version).get("groups", {}).get(group, {}).get(key)
        snap = self.get(project, branch)
        return snap.get("groups", {}).get(group, {}).get(key)

    def list_members(self):
        """集群成员（gRPC 通道；dev-single 返回空列表）。"""
        if self._grpc_stub is None:
            raise ConfigError("NO_GRPC", "需要 gRPC 端点")
        from config import v1_pb2

        resp = self._grpc().ListMembers(v1_pb2.ListMembersRequest(), metadata=self._meta)
        return [
            {
                "node_id": m.node_id,
                "grpc_addr": m.grpc_addr,
                "http_addr": m.http_addr,
                "is_leader": m.is_leader,
                "is_voter": m.is_voter,
                "committed_index": m.committed_index,
            }
            for m in resp.members
        ]

    def watch(self, project, branch, listener, stop=None):
        """订阅发布事件；断线以 after_version 续传重连。
        listener(event)；stop: threading.Event。gRPC 事件含 snapshot_required。
        """
        if self._grpc_stub is not None:
            self._watch_grpc(project, branch, listener, stop)
            return
        self._watch_http(project, branch, listener, stop)

    def _watch_grpc(self, project, branch, listener, stop):
        from config import v1_pb2

        after = 0
        while not (stop and stop.is_set()):
            try:
                for e in self._grpc().Watch(
                    v1_pb2.WatchRequest(project=project, branch=branch, after_version=after),
                    metadata=self._meta,
                ):
                    if stop and stop.is_set():
                        return
                    after = max(after, e.version)
                    listener(
                        {
                            "version": e.version,
                            "ty": ["", "value_publish", "structure_publish", "shared_cascade", "rollback"][e.type],
                            "structure_version": e.structure_version,
                            "comment": e.comment,
                            "request_id": e.request_id,
                            "changes": [
                                {
                                    "group": c.group,
                                    "key": c.key,
                                    "kind": "delete" if c.kind == 2 else "upsert",
                                    "new_value": _value_from_proto(c.new_value) if c.HasField("new_value") else None,
                                }
                                for c in e.changes
                            ],
                            "snapshot_required": e.snapshot_required,
                        }
                    )
            except Exception:
                if stop and stop.is_set():
                    return
                time.sleep(min(BACKOFF_BASE_MS * 2, 15000) / 1000)

    def _watch_http(self, project, branch, listener, stop):
        path = "/v1/projects/%s/branches/%s/watch" % (project, branch)
        attempt = 0
        while not (stop and stop.is_set()):
            if attempt > 0:
                time.sleep(min(BACKOFF_BASE_MS * (2 ** attempt), 15000) / 1000)
            attempt += 1
            try:
                url = self.endpoints[0] if isinstance(self.endpoints[0], str) else self.endpoints[0].get("http")
                with urllib.request.urlopen(url + path, timeout=None) as r:
                    for raw in r:
                        line = raw.decode().strip()
                        if line.startswith("data:"):
                            try:
                                listener(json.loads(line[5:].strip()))
                            except ValueError:
                                pass
            except (urllib.error.URLError, OSError):
                if stop and stop.is_set():
                    break
