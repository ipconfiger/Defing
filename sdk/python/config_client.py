"""Defing ConfigClient — Python SDK（urllib 标准库 + SSE）。"""

import json
import time
import urllib.error
import urllib.request

BACKOFF_BASE_MS = 200


class ConfigError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


class ConfigClient:
    def __init__(self, endpoints):
        self.endpoints = endpoints

    def _request(self, path):
        last = None
        for i, ep in enumerate(self.endpoints):
            try:
                with urllib.request.urlopen(ep + path, timeout=5) as r:
                    return json.loads(r.read().decode())
            except (urllib.error.URLError, OSError) as e:
                last = e
                time.sleep((i + 1) * BACKOFF_BASE_MS / 1000)
        raise ConfigError("NO_ENDPOINT", "all endpoints unreachable: %s" % last)

    def get(self, project, branch):
        return self._request("/v1/projects/%s/branches/%s/snapshot" % (project, branch))

    def get_item(self, project, branch, group, key):
        snap = self.get(project, branch)
        return snap.get("groups", {}).get(group, {}).get(key)

    def watch(self, project, branch, listener, stop=None):
        """订阅发布事件；断线自动重连。listener(event)；stop: threading.Event。"""
        path = "/v1/projects/%s/branches/%s/watch" % (project, branch)
        attempt = 0
        while not (stop and stop.is_set()):
            if attempt > 0:
                time.sleep(min(BACKOFF_BASE_MS * (2 ** attempt), 15000) / 1000)
            attempt += 1
            try:
                with urllib.request.urlopen(self.endpoints[0] + path, timeout=None) as r:
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
