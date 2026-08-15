#!/usr/bin/env python3
"""dsh 配置中心接入示例站点。

一次性示例代码：按仓库规约豁免严格 TDD，正确性以 docker 端到端实测为准。

配置项存放在 dsh（project=example-site, branch=dev, group=site）：
    title           站点标题（支持 watch 热更新）
    login_username  登录账号
    login_password  登录密码

环境变量：
    PORT            站点监听端口（默认 8000）
    DSH_ENDPOINT    dsh 地址（默认 http://dsh:8384）
    DSH_PROJECT / DSH_BRANCH / DSH_GROUP   配置坐标（默认 example-site / dev / site）
    以下仅 setup 子命令使用：
    ADMIN_PASSWORD  dsh 管理员密码
    TITLE / LOGIN_USERNAME / LOGIN_PASSWORD  写入 dsh 的初始配置值

用法：
    python3 app.py serve   # 启动站点：拉取配置 + watch 热更新
    python3 app.py setup   # 通过 admin API 初始化 project 与配置并发布
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import html
import json
import os
import secrets
import signal
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from collections import deque
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable

# --- SDK 路径：容器内 /app/sdk；本地运行时 <repo>/sdk/python ---
for _cand in ("/app/sdk", os.path.join(os.path.dirname(__file__), "..", "sdk", "python")):
    if os.path.isfile(os.path.join(_cand, "config_client.py")):
        sys.path.insert(0, _cand)
        break

from config_client import ConfigClient  # noqa: E402  (路径注入后导入)

PORT = int(os.environ.get("PORT", "8000"))
DSH_ENDPOINT = os.environ.get("DSH_ENDPOINT", "http://dsh:8384")
DSH_PROJECT = os.environ.get("DSH_PROJECT", "example-site")
DSH_BRANCH = os.environ.get("DSH_BRANCH", "dev")
DSH_GROUP = os.environ.get("DSH_GROUP", "site")


# ---------------------------------------------------------------------------
# dsh 管理面 API（仅 setup 使用；运行期站点只走数据面 SDK）
# ---------------------------------------------------------------------------
class AdminApi:
    def __init__(self, base_url: str, password: str) -> None:
        self._base = base_url.rstrip("/")
        self._password = password
        self._token: str | None = None

    def _request(self, method: str, path: str, body: dict[str, Any] | None = None,
                 auth: bool = True) -> Any:
        req = urllib.request.Request(
            self._base + path,
            data=json.dumps(body).encode() if body is not None else None,
            method=method,
            headers={"Content-Type": "application/json"},
        )
        if auth:
            if self._token is None:
                raise RuntimeError("not logged in")
            req.add_header("Authorization", f"Bearer {self._token}")
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                raw = resp.read()
        except urllib.error.HTTPError as e:
            detail = e.read().decode(errors="replace")
            raise RuntimeError(f"{method} {path} -> HTTP {e.code}: {detail}") from e
        return json.loads(raw) if raw else {}

    def login(self) -> None:
        resp = self._request("POST", "/api/v1/login", {"password": self._password}, auth=False)
        self._token = resp["token"]

    def logout(self) -> None:
        try:
            self._request("POST", "/api/v1/logout")
        except RuntimeError:
            pass  # 登出失败不阻塞流程（token 有 TTL）
        self._token = None

    def project_exists(self, name: str) -> bool:
        projects = self._request("GET", "/api/v1/projects")
        return any(p.get("id") == name or p.get("name") == name for p in projects)

    def create_project(self, name: str) -> None:
        self._request("POST", "/api/v1/projects", {"name": name})

    def get_structure_version(self) -> int:
        st = self._request("GET", f"/api/v1/projects/{DSH_PROJECT}/branches/{DSH_BRANCH}")
        v = st.get("structure_version")
        return v if isinstance(v, int) else 0

    def publish_structure(self, base_version: int) -> None:
        """定义 site 组结构（title/login_username/login_password 三个 string 项）。"""
        groups = [
            {"name": DSH_GROUP, "items": [
                {"key": k, "type": "string", "required": True, "secret": False}
                for k in ("title", "login_username", "login_password")
            ]}
        ]
        self._request("PUT", f"/api/v1/projects/{DSH_PROJECT}/structure-draft",
                      {"base_version": base_version, "groups": groups})
        self._request("POST", f"/api/v1/projects/{DSH_PROJECT}/structure-draft/publish",
                      {"comment": "example-site 初始化结构", "request_id": str(uuid.uuid4())})
        print(f"[setup] 结构已发布(base={base_version}): {DSH_GROUP}/title,login_username,login_password")

    def write_draft_and_publish(self, updates: list[dict[str, Any]], comment: str) -> int:
        draft_path = f"/api/v1/projects/{DSH_PROJECT}/branches/{DSH_BRANCH}/draft"
        pub_path = f"/api/v1/projects/{DSH_PROJECT}/branches/{DSH_BRANCH}/publish"
        self._request("PUT", draft_path, {"updates": updates, "deletes": []})
        resp = self._request("POST", pub_path, {"comment": comment, "request_id": str(uuid.uuid4())})
        return int(resp["version"])


def _str_value(v: str) -> dict[str, Any]:
    return {"type": "string", "str_value": v}


_REQUIRED_KEYS = ("title", "login_username", "login_password")


def _site_updates(title: str, username: str, password: str) -> list[dict[str, Any]]:
    return [
        {"group": DSH_GROUP, "key": "title", "value": _str_value(title)},
        {"group": DSH_GROUP, "key": "login_username", "value": _str_value(username)},
        {"group": DSH_GROUP, "key": "login_password", "value": _str_value(password)},
    ]


def set_title_command() -> int:
    """运行中修改 title：读当前配置 → 全量写 draft（含所有 required 项）→ 发布 → 登出。

    用法: python3 app.py set-title "新标题"
    """
    if len(sys.argv) < 3:
        print("用法: app.py set-title <新标题>", file=sys.stderr)
        return 2
    new_title = sys.argv[2]

    client = ConfigClient([DSH_ENDPOINT])
    snap = client.get(DSH_PROJECT, DSH_BRANCH)
    group = (snap.get("groups") or {}).get(DSH_GROUP) or {}
    api = AdminApi(DSH_ENDPOINT, os.environ.get("ADMIN_PASSWORD", ""))
    api.login()
    try:
        version = api.write_draft_and_publish(
            _site_updates(new_title,
                          str(group.get("login_username", "")),
                          str(group.get("login_password", ""))),
            comment=f"运行中修改 title -> {new_title!r}",
        )
        print(f"[set-title] 已发布 version={version}, 新 title={new_title!r}")
        return 0
    finally:
        api.logout()


def setup_command() -> int:
    """初始化 dsh 配置：建 project（幂等）→ 写草稿 → 发布。"""
    api = AdminApi(DSH_ENDPOINT, os.environ.get("ADMIN_PASSWORD", ""))
    title = os.environ.get("TITLE", "dsh 演示站")
    username = os.environ.get("LOGIN_USERNAME", "admin")
    password = os.environ.get("LOGIN_PASSWORD", "example-pass-123")

    try:
        api.login()
    except RuntimeError as e:
        if "409" in str(e):
            print("[setup] 登录被拒（409，已有管理员会话）。请等待会话过期或清理 dsh 数据目录后重试。",
                  file=sys.stderr)
        raise
    try:
        if api.project_exists(DSH_PROJECT):
            print(f"[setup] project '{DSH_PROJECT}' 已存在，跳过创建")
        else:
            api.create_project(DSH_PROJECT)
            print(f"[setup] 已创建 project '{DSH_PROJECT}'（含默认分支）")

        try:
            version = api.write_draft_and_publish(
                [
                    {"group": DSH_GROUP, "key": "title", "value": _str_value(title)},
                    {"group": DSH_GROUP, "key": "login_username", "value": _str_value(username)},
                    {"group": DSH_GROUP, "key": "login_password", "value": _str_value(password)},
                ],
                comment="example-site 初始化配置",
            )
        except RuntimeError as e:
            if "unknown item" not in str(e):
                raise
            # 结构未定义：先发布结构（CAS 基线=当前已发布结构版本），再重试写值
            api.publish_structure(api.get_structure_version())
            version = api.write_draft_and_publish(
                [
                    {"group": DSH_GROUP, "key": "title", "value": _str_value(title)},
                    {"group": DSH_GROUP, "key": "login_username", "value": _str_value(username)},
                    {"group": DSH_GROUP, "key": "login_password", "value": _str_value(password)},
                ],
                comment="example-site 初始化配置",
            )
        print(f"[setup] 配置已发布: version={version}, "
              f"title={title!r}, login_username={username!r}")
        return 0
    finally:
        api.logout()  # 单管理员会话策略：用完即还


# ---------------------------------------------------------------------------
# 运行期状态：初始快照 + watch 增量更新（线程安全）
# ---------------------------------------------------------------------------
def _unwrap(value: Any) -> Any:
    """watch 事件的 new_value 是带类型标签的 Value 对象，解包为原始值。"""
    if isinstance(value, dict) and "type" in value:
        for field in ("str_value", "int_value", "float_value", "bool_value", "json_value"):
            if field in value:
                return value[field]
        if "ciphertext" in value:
            return "***"
        return None
    return value


def _apply_config_value(state_key: str, new_value: Any) -> tuple[bool, Any]:
    unwrapped = _unwrap(new_value)
    if unwrapped is None or unwrapped == "":
        return False, None
    return True, unwrapped


@dataclass
class SiteState:
    title: str = "(未初始化)"
    login_username: str = ""
    login_password: str = ""
    version: int = 0
    loaded_from_dsh: bool = False
    started_at: float = field(default_factory=time.time)
    watch_events: deque[dict[str, Any]] = field(default_factory=lambda: deque(maxlen=50))
    _lock: threading.Lock = field(default_factory=threading.Lock, repr=False)

    def apply_snapshot(self, snap: dict[str, Any]) -> None:
        group = (snap.get("groups") or {}).get(DSH_GROUP) or {}
        with self._lock:
            self.title = str(group.get("title", self.title))
            self.login_username = str(group.get("login_username", self.login_username))
            self.login_password = str(group.get("login_password", self.login_password))
            self.version = int(snap.get("version", self.version))
            self.loaded_from_dsh = True

    def apply_event(self, event: dict[str, Any]) -> None:
        for change in event.get("changes") or []:
            if change.get("group") != DSH_GROUP:
                continue
            key, raw_value = change.get("key"), change.get("new_value")
            ok, value = _apply_config_value(key, raw_value)
            if not ok:
                continue
            attr = {"title": "title", "login_username": "login_username",
                    "login_password": "login_password"}.get(key)
            if attr:
                with self._lock:
                    setattr(self, attr, str(value))
        with self._lock:
            self.version = max(self.version, int(event.get("version", 0)))
            self.watch_events.append({
                "at": time.strftime("%H:%M:%S"),
                "version": event.get("version"),
                "type": event.get("ty") or event.get("type"),
                "changes": [
                    {"key": c.get("key"), "kind": c.get("kind")}
                    for c in (event.get("changes") or [])
                ],
            })

    def snapshot_view(self) -> dict[str, Any]:
        with self._lock:
            return {
                "title": self.title,
                "login_username": self.login_username,
                "password_fingerprint": hashlib.sha256(
                    self.login_password.encode()).hexdigest()[:8],
                "config_version": self.version,
                "loaded_from_dsh": self.loaded_from_dsh,
                "uptime_seconds": round(time.time() - self.started_at, 1),
                "watch_events": list(self.watch_events),
            }


STATE = SiteState()
_SESSIONS: dict[str, str] = {}  # token -> username（内存态，重启即失效）
_SESSION_SECRET = secrets.token_bytes(32)


def _issue_session(username: str) -> str:
    token = secrets.token_urlsafe(24)
    _SESSIONS[token] = username
    return token


def _check_session(token: str) -> str | None:
    return _SESSIONS.get(token)


# ---------------------------------------------------------------------------
# HTTP 站点（stdlib）
# ---------------------------------------------------------------------------
_PAGE = """<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8">
<title>{title}</title>
<style>
 body {{ font-family: system-ui, sans-serif; max-width: 640px; margin: 64px auto;
        color: #1f2933; }} h1 {{ color: #0b6bcb; }}
 form {{ display: grid; gap: 10px; max-width: 280px; margin-top: 24px; }}
 input, button {{ padding: 8px; font-size: 14px; }}
 .meta {{ color: #616e7c; font-size: 13px; margin-top: 32px; }}
</style></head>
<body>
<h1>{title}</h1>
{body}
<p class="meta">配置来源: dsh ({project}/{branch}/{group}, version={version}) &middot; watch 事件: {events}</p>
</body></html>"""

_LOGIN_FORM = """<form method="post" action="/login">
<label>账号 <input name="username" autocomplete="username"></label>
<label>密码 <input name="password" type="password" autocomplete="current-password"></label>
<button type="submit">登录</button>
</form>"""


class Handler(BaseHTTPRequestHandler):
    server_version = "dsh-example/1.0"

    def _send(self, status: int, body: bytes, content_type: str,
              headers: dict[str, str] | None = None) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def _session_user(self) -> str | None:
        cookie = self.headers.get("Cookie", "")
        for part in cookie.split(";"):
            if part.strip().startswith("session="):
                return _check_session(part.strip().split("=", 1)[1])
        return None

    def do_GET(self) -> None:  # noqa: N802
        path = self.path.split("?", 1)[0]
        view = STATE.snapshot_view()
        if path == "/healthz":
            self._send(200, b'{"status":"ok"}', "application/json")
        elif path == "/api/state":
            body = json.dumps(view, ensure_ascii=False, indent=2).encode()
            self._send(200, body, "application/json; charset=utf-8")
        elif path == "/":
            user = self._session_user()
            body_html = (f"<p>已登录: {html.escape(user)} <a href='/logout'>退出</a></p>"
                         if user else _LOGIN_FORM)
            page = _PAGE.format(
                title=html.escape(view["title"]),
                body=body_html,
                project=DSH_PROJECT, branch=DSH_BRANCH, group=DSH_GROUP,
                version=view["config_version"],
                events=len(view["watch_events"]),
            )
            self._send(200, page.encode(), "text/html; charset=utf-8")
        else:
            self._send(404, b"not found", "text/plain; charset=utf-8")

    def do_POST(self) -> None:  # noqa: N802
        path = self.path.split("?", 1)[0]
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length).decode() if length else ""
        form: dict[str, str] = {}
        for pair in raw.split("&"):
            if "=" in pair:
                k, _, v = pair.partition("=")
                form[k] = urllib.parse.unquote(v.replace("+", " "))

        if path == "/login":
            view = STATE.snapshot_view()
            expected = hmac.new(_SESSION_SECRET, b"", hashlib.sha256)  # 占位，防时序比较告警
            del expected
            if (hmac.compare_digest(form.get("username", ""), view["login_username"])
                    and hmac.compare_digest(form.get("password", ""), STATE.login_password)):
                token = _issue_session(form["username"])
                self._send(303, b"", "text/plain",
                           {"Set-Cookie": f"session={token}; HttpOnly; Path=/",
                            "Location": "/"})
            else:
                page = _PAGE.format(title=html.escape(view["title"]),
                                    body="<p style='color:#c0392b'>账号或密码错误</p>" + _LOGIN_FORM,
                                    project=DSH_PROJECT, branch=DSH_BRANCH, group=DSH_GROUP,
                                    version=view["config_version"], events=0)
                self._send(401, page.encode(), "text/html; charset=utf-8")
        elif path == "/logout":
            cookie = self.headers.get("Cookie", "")
            for part in cookie.split(";"):
                if part.strip().startswith("session="):
                    _SESSIONS.pop(part.strip().split("=", 1)[1], None)
            self._send(303, b"", "text/plain",
                         {"Set-Cookie": "session=; Path=/; Max-Age=0", "Location": "/"})
        else:
            self._send(404, b"not found", "text/plain; charset=utf-8")

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        print(f"[http] {self.address_string()} {format % args}", flush=True)


def serve_command() -> int:
    client = ConfigClient([DSH_ENDPOINT])
    print(f"[serve] dsh={DSH_ENDPOINT} project={DSH_PROJECT}/{DSH_BRANCH}/{DSH_GROUP}")

    try:
        snap = client.get(DSH_PROJECT, DSH_BRANCH)
        STATE.apply_snapshot(snap)
        view = STATE.snapshot_view()
        print(f"[serve] 初始配置加载成功 version={view['config_version']} "
              f"title={view['title']!r} username={view['login_username']!r}")
    except Exception as e:  # dsh 不可用不致命：watch 线程会持续重试
        print(f"[serve] 初始快照拉取失败（将继续通过 watch 重试）: {e}", file=sys.stderr)

    def on_event(event: dict[str, Any]) -> None:
        try:
            STATE.apply_event(event)
            keys = [c.get("key") for c in (event.get("changes") or [])]
            print(f"[watch] version={event.get('version')} changes={keys} -> "
                  f"title={STATE.title!r}", flush=True)
        except Exception as e:  # 回调内异常不能打断 watch 线程
            print(f"[watch] 事件处理异常: {e}", file=sys.stderr)

    stop = threading.Event()
    watcher = threading.Thread(
        target=client.watch, args=(DSH_PROJECT, DSH_BRANCH, on_event, stop),
        daemon=True, name="dsh-watch")
    watcher.start()

    server = ThreadingHTTPServer(("0.0.0.0", PORT), Handler)

    def _shutdown(signum: int, _frame: Any) -> None:
        print(f"[serve] 收到信号 {signum}，退出", flush=True)
        stop.set()
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, _shutdown)
    signal.signal(signal.SIGINT, _shutdown)
    print(f"[serve] 站点已启动: http://0.0.0.0:{PORT}", flush=True)
    server.serve_forever()
    server.server_close()
    return 0


def main() -> int:
    cmd = sys.argv[1] if len(sys.argv) > 1 else "serve"
    commands: dict[str, Callable[[], int]] = {
        "serve": serve_command, "setup": setup_command, "set-title": set_title_command}
    if cmd not in commands:
        print(f"未知子命令: {cmd}（可用: {'/'.join(commands)}）", file=sys.stderr)
        return 2
    return commands[cmd]()


if __name__ == "__main__":
    raise SystemExit(main())
