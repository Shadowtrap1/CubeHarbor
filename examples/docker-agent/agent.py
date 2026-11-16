#!/usr/bin/env python3
"""Bridge between this host's docker compose Minecraft stacks and a cubeharbor dashboard.

Lists containers through the Docker socket, picks the Minecraft servers, pings each one with the
Minecraft status protocol and serves the result at /api/servers in the format cubeharbor's
`dashboard.remotes` reads. It never starts, stops or changes a container.

With TOKEN set it also offers each server's console to callers that send
`Authorization: Bearer <TOKEN>`:
  GET  /api/servers/<name>/logs?tail=N   the container's log, followed live (plain text lines)
  POST /api/servers/<name>/command       body: one server command, run with `rcon-cli` inside
                                         the container (`docker exec`); replies with its output
Without TOKEN those two answer 404 and the list is all there is.

Environment:
  LISTEN        address:port to serve on                     (default 0.0.0.0:8765)
  PUBLIC_HOST   host name or IP players use for this machine (shown as the address)
  PING_HOST     where published ports are reachable from here (default host.docker.internal)
  IMAGES        comma separated image name fragments to treat as Minecraft servers
                                                              (default itzg/minecraft-server)
  REFRESH       seconds between scans                         (default 10)
  DOCKER_SOCK   Docker socket path                            (default /var/run/docker.sock)
  TOKEN         shared secret that unlocks logs and commands (at least 24 characters)
"""
import hmac
import http.client
import json
import os
import re
import select
import socket
import struct
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LISTEN = os.environ.get("LISTEN", "0.0.0.0:8765")
PUBLIC_HOST = os.environ.get("PUBLIC_HOST", "") or socket.gethostname()
PING_HOST = os.environ.get("PING_HOST", "host.docker.internal")
IMAGES = [s.strip() for s in os.environ.get("IMAGES", "itzg/minecraft-server").split(",") if s.strip()]
REFRESH = max(2, int(os.environ.get("REFRESH", "10")))
DOCKER_SOCK = os.environ.get("DOCKER_SOCK", "/var/run/docker.sock")
TOKEN = os.environ.get("TOKEN", "").strip()
if TOKEN and len(TOKEN) < 24:
    print("TOKEN is shorter than 24 characters: console disabled", flush=True)
    TOKEN = ""
MC_PORT = 25565
MAX_LINE = 64 * 1024
ANSI = re.compile(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[@-Z\\-_])")


class DockerConn(http.client.HTTPConnection):
    def __init__(self, timeout=5):
        super().__init__("localhost", timeout=timeout)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(DOCKER_SOCK)


def docker_get(path):
    conn = DockerConn()
    try:
        conn.request("GET", path)
        resp = conn.getresponse()
        body = resp.read()
        if resp.status != 200:
            raise RuntimeError(f"docker GET {path}: HTTP {resp.status}: {body[:200]!r}")
        return json.loads(body)
    finally:
        conn.close()


def docker_post(path, payload, timeout=5):
    """POST JSON, return (status, body bytes). The body is read until Docker closes it."""
    conn = DockerConn(timeout)
    try:
        conn.request("POST", path, json.dumps(payload), {"Content-Type": "application/json"})
        resp = conn.getresponse()
        return resp.status, resp.read()
    finally:
        conn.close()


def run_command(container_id, command):
    """Run one server command through rcon-cli inside the container and return its output."""
    status, body = docker_post(
        f"/containers/{container_id}/exec",
        {"Cmd": ["rcon-cli", command], "AttachStdout": True, "AttachStderr": True, "Tty": True},
    )
    if status != 201:
        raise RuntimeError(f"docker exec: HTTP {status}: {body[:200].decode(errors='replace')}")
    exec_id = json.loads(body)["Id"]
    status, out = docker_post(f"/exec/{exec_id}/start", {"Detach": False, "Tty": True}, timeout=20)
    if status != 200:
        raise RuntimeError(f"docker exec start: HTTP {status}: {out[:200].decode(errors='replace')}")
    return clean_line(out.decode(errors="replace").replace("\r\n", "\n"), keep_newlines=True).strip()


def clean_line(text, keep_newlines=False):
    """Terminal output as plain text: no colour codes, and only what a redrawn line ends up showing."""
    text = ANSI.sub("", text)
    if keep_newlines:
        return "\n".join(clean_line(part) for part in text.split("\n"))
    parts = [p for p in text.split("\r") if p.strip()]
    return (parts[-1] if parts else "").rstrip()


def open_log(container_id, tail):
    """Follow a container's log. Returns (socket, bytes already read past the headers, tty).

    HTTP/1.0 keeps Docker from chunking the reply, so everything after the headers is the log
    and the socket can be watched with select().
    """
    tty = bool((docker_get(f"/containers/{container_id}/json").get("Config") or {}).get("Tty"))
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.settimeout(5)
        sock.connect(DOCKER_SOCK)
        path = f"/containers/{container_id}/logs?follow=true&stdout=true&stderr=true&tail={tail}"
        sock.sendall(f"GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n".encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = sock.recv(4096)
            if not chunk:
                raise RuntimeError("docker closed the log connection")
            head += chunk
            if len(head) > 65536:
                raise RuntimeError("docker log reply has no end of headers")
        head, rest = head.split(b"\r\n\r\n", 1)
        status = head.split(b"\r\n", 1)[0].split()
        if len(status) < 2 or status[1] != b"200":
            raise RuntimeError(f"docker logs: {head.splitlines()[0].decode(errors='replace')}")
        return sock, rest, tty
    except Exception:
        sock.close()
        raise


class Demux:
    """Docker's stream framing for containers without a TTY: 8 byte header, then the payload."""

    def __init__(self):
        self.buf = b""

    def feed(self, data):
        self.buf += data
        out = b""
        while len(self.buf) >= 8:
            size = struct.unpack(">I", self.buf[4:8])[0]
            if len(self.buf) < 8 + size:
                break
            out += self.buf[8:8 + size]
            self.buf = self.buf[8 + size:]
        return out


# --- Minecraft server list ping -------------------------------------------------------------


def varint(n):
    out = b""
    while True:
        b = n & 0x7F
        n >>= 7
        out += bytes([b | (0x80 if n else 0)])
        if not n:
            return out


def read_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("connection closed")
        buf += chunk
    return buf


def read_varint(sock):
    n = 0
    for shift in range(0, 35, 7):
        b = read_exact(sock, 1)[0]
        n |= (b & 0x7F) << shift
        if not b & 0x80:
            return n
    raise ValueError("varint too long")


def parse_varint(data, pos):
    n = 0
    for shift in range(0, 35, 7):
        b = data[pos]
        pos += 1
        n |= (b & 0x7F) << shift
        if not b & 0x80:
            return n, pos
    raise ValueError("varint too long")


def flatten(text):
    """A chat component (string, object or list) as plain text."""
    if isinstance(text, str):
        return text
    if isinstance(text, list):
        return "".join(flatten(t) for t in text)
    if isinstance(text, dict):
        return str(text.get("text", "")) + flatten(text.get("extra", []))
    return ""


def strip_codes(s):
    out, skip = [], False
    for ch in s:
        if skip:
            skip = False
        elif ch == "§":
            skip = True
        else:
            out.append(ch)
    return " ".join("".join(out).split())


def ping(host, port, timeout=3):
    with socket.create_connection((host, port), timeout=timeout) as sock:
        sock.settimeout(timeout)
        name = host.encode()
        handshake = b"\x00" + varint(767) + varint(len(name)) + name + struct.pack(">H", port) + b"\x01"
        sock.sendall(varint(len(handshake)) + handshake + b"\x01\x00")
        length = read_varint(sock)
        if not 0 < length <= 4 * 1024 * 1024:
            raise ValueError(f"bad status length {length}")
        data = read_exact(sock, length)
    _packet_id, pos = parse_varint(data, 0)
    strlen, pos = parse_varint(data, pos)
    return json.loads(data[pos:pos + strlen])


# --- Discovery ------------------------------------------------------------------------------


def is_minecraft(container):
    image = container.get("Image", "")
    return any(frag in image for frag in IMAGES)


def host_port(inspect, private, proto):
    """Host port a container port is published on (works for stopped containers too)."""
    key = f"{private}/{proto}"
    for source in (
        (inspect.get("NetworkSettings") or {}).get("Ports") or {},
        (inspect.get("HostConfig") or {}).get("PortBindings") or {},
    ):
        for binding in source.get(key) or []:
            if binding.get("HostPort"):
                return int(binding["HostPort"])
    return None


def describe(container, names):
    labels = container.get("Labels") or {}
    inspect = docker_get(f"/containers/{container['Id']}/json")
    env = dict(e.split("=", 1) for e in (inspect.get("Config") or {}).get("Env") or [] if "=" in e)
    project = labels.get("com.docker.compose.project")
    service = labels.get("com.docker.compose.service")
    name = project or (container.get("Names") or ["/?"])[0].lstrip("/")
    if project and names.count(project) > 1 and service:
        name = f"{project}-{service}"

    state = container.get("State", "")
    health = ((inspect.get("State") or {}).get("Health") or {}).get("Status")
    port = host_port(inspect, int(env.get("SERVER_PORT", MC_PORT)), "tcp")
    server = {
        "name": name,
        "type": "minecraft",
        "online": False,
        "phase": "Offline" if state != "running" else "Starting",
        "project": project,
        "loader": (env.get("TYPE") or "vanilla").lower(),
        "version": env.get("VERSION"),
        "memory": env.get("MEMORY") or env.get("MAX_MEMORY"),
        "console": bool(TOKEN),
    }
    if port:
        server["address"] = PUBLIC_HOST if port == MC_PORT else f"{PUBLIC_HOST}:{port}"
    if state != "running":
        server["message"] = container.get("Status")
    elif port:
        try:
            status = ping(PING_HOST, port)
            players = status.get("players") or {}
            server.update(
                online=True,
                phase="Online",
                players={"online": int(players.get("online", 0)), "max": int(players.get("max", 0))},
                motd=strip_codes(flatten(status.get("description"))),
            )
            if not server["version"] or server["version"].upper() in ("LATEST", "SNAPSHOT"):
                server["version"] = (status.get("version") or {}).get("name")
        except Exception as e:  # still starting, or the port isn't answering
            server["message"] = f"container {health or state}, no status reply: {e}"
    else:
        server["message"] = f"container {health or state}, port {MC_PORT} is not published"
    return {k: v for k, v in server.items() if v is not None}


def scan():
    containers = [c for c in docker_get("/containers/json?all=true") if is_minecraft(c)]
    names = [(c.get("Labels") or {}).get("com.docker.compose.project") for c in containers]
    servers, ids = [], {}
    for c in containers:
        try:
            server = describe(c, names)
            servers.append(server)
            ids[server["name"]] = c["Id"]
        except Exception as e:
            print(f"skipping {c.get('Names')}: {e}", flush=True)
    state["ids"] = ids
    servers.sort(key=lambda s: s["name"])
    online = [s for s in servers if s["online"]]
    return {
        "updated": int(time.time()),
        "servers": servers,
        "totals": {
            "servers": len(servers),
            "online": len(online),
            "players": sum(s.get("players", {}).get("online", 0) for s in online),
        },
    }


state = {"body": None, "error": "first scan has not finished", "ids": {}}


def refresh_loop():
    last = None
    while True:
        try:
            result = scan()
            state["body"], state["error"] = json.dumps(result).encode(), None
            summary = [(s["name"], s["phase"]) for s in result["servers"]]
        except Exception as e:
            state["error"] = str(e)
            summary = f"error: {e}"
        if summary != last:
            print(f"servers: {summary}", flush=True)
            last = summary
        time.sleep(REFRESH)


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        path, _, query = self.path.partition("?")
        seg = path.strip("/").split("/")
        if seg == ["healthz"]:
            self.reply(200, b'{"ok":true}')
        elif seg in (["api", "servers"], ["api", "v1", "servers"]):
            if state["body"] is None:
                self.reply(503, json.dumps({"error": state["error"]}).encode())
            else:
                self.reply(200, state["body"])
        elif len(seg) == 4 and seg[:2] == ["api", "servers"] and seg[3] == "logs":
            container = self.console_target(seg[2])
            if container:
                self.logs(container, query)
        else:
            self.error(404, "not found")

    def do_POST(self):
        seg = self.path.partition("?")[0].strip("/").split("/")
        if len(seg) == 4 and seg[:2] == ["api", "servers"] and seg[3] == "command":
            container = self.console_target(seg[2])
            if container:
                self.send_command(seg[2], container)
        else:
            self.error(404, "not found")

    def console_target(self, name):
        """The container id for a console request, after checking the token. Replies on failure."""
        if not TOKEN:
            self.error(404, "console is not enabled on this agent (no TOKEN)")
            return None
        sent = self.headers.get("Authorization", "")
        if not hmac.compare_digest(sent.encode(), f"Bearer {TOKEN}".encode()):
            self.error(401, "unauthorized")
            return None
        container = state["ids"].get(name)
        if not container:
            self.error(404, f"no server named {name}")
        return container

    def logs(self, container, query):
        tail = 200
        for kv in query.split("&"):
            k, _, v = kv.partition("=")
            if k == "tail" and v.isdigit():
                tail = min(int(v), 5000)
        try:
            sock, pending, tty = open_log(container, tail)
        except Exception as e:
            self.error(502, str(e))
            return
        try:
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            demux, buf = Demux(), b""
            while True:
                if pending:
                    buf += pending if tty else demux.feed(pending)
                    *lines, buf = buf.split(b"\n")
                    if len(buf) > MAX_LINE:
                        lines, buf = lines + [buf], b""
                    out = b"".join(clean_line(l.decode(errors="replace")).encode() + b"\n" for l in lines)
                    if out:
                        self.wfile.write(out)
                # Wake up for new log data, or when the caller goes away while the log is quiet.
                readable, _, _ = select.select([sock, self.connection], [], [], 60)
                if self.connection in readable and not self.connection.recv(1, socket.MSG_PEEK):
                    return
                pending = b""
                if sock in readable:
                    pending = sock.recv(65536)
                    if not pending:  # container stopped
                        return
        except OSError:
            pass  # caller disconnected
        finally:
            sock.close()

    def send_command(self, name, container):
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            length = 0
        if not 0 < length <= 4096:
            self.error(400, "send one command as the request body (at most 4096 bytes)")
            return
        command = " ".join(self.rfile.read(length).decode(errors="replace").split()).lstrip("/")
        if not command:
            self.error(400, "empty command")
            return
        print(f"command on {name} from {self.client_address[0]}: {command}", flush=True)
        try:
            out = run_command(container, command)
        except Exception as e:
            self.error(502, str(e))
            return
        self.reply(200, out.encode(), "text/plain; charset=utf-8")

    def error(self, code, message):
        self.reply(code, json.dumps({"error": message}).encode())

    def reply(self, code, body, content_type="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    host, _, port = LISTEN.rpartition(":")
    threading.Thread(target=refresh_loop, daemon=True).start()
    print(f"cubeharbor-agent on {LISTEN}, images {IMAGES}, address {PUBLIC_HOST}, console {'on' if TOKEN else 'off'}", flush=True)
    ThreadingHTTPServer((host or "0.0.0.0", int(port)), Handler).serve_forever()
