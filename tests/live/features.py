#!/usr/bin/env python3
"""Opt-in live scenarios for the optional tools and image input.

Each scenario is one `plak run` (two for the image-generation round trip) in
its own small git repo, with a private `PLAK_HOME` whose root template lists
exactly the optional tools the scenario needs. Oracles read the session log
through `plak session log <id> --json`, the repository, the raw exchange
capture and the MCP servers' own records; the model's claims are never the
oracle. Selected with `just smoke <route> --only <names>`; see README.md.

A route row that refuses a tool or image input at bind or admission is
`refused`, distinct from a failure: it is the current product statement, not
a broken run.
"""

from __future__ import annotations

import base64
import hashlib
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

import live
from live import HARNESS, MODEL, PROVIDER, ScenarioFailure

REFUSED = "refused"

CATALOG = (
    ("image_input", "cli"),
    ("fetch_url", "cli"),
    ("mcp_stdio", "cli"),
    ("mcp_http", "cli"),
    ("mcp_github_stdio", "cli"),
    ("mcp_github_http", "cli"),
    ("mcp_everything_all", "cli"),
    ("mcp_github_all_stdio", "cli"),
    ("mcp_github_all_http", "cli"),
    ("web_search", "cli"),
    ("web_fetch", "cli"),
    ("x_search", "cli"),
    ("image_gen_roundtrip", "cli"),
)

BROWSER_UA = ("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
              "(KHTML, like Gecko) Chrome/141.0 Safari/537.36")
CAT_URL = "https://upload.wikimedia.org/wikipedia/commons/3/3a/Cat03.jpg"
IMAGE_LIMIT = 3_932_160  # 3.75 MiB, the product's per-image cap

PUBLIC_REPO = "N0xMare/plak"
# A small tracked file that records one SHA-256 no model can guess.
MARKER_PATH = "vendor/monty-typeshed/PLAK-PATCH.md"
SHA256_HEX = re.compile(r"\b[0-9a-f]{64}\b")

EVERYTHING_PACKAGE = "@modelcontextprotocol/server-everything"
EVERYTHING_VERSION = "1.0.0"
GITHUB_MCP_IMAGE = ("ghcr.io/github/github-mcp-server@sha256:"
                    "ffced0d76e77428532a2ced185516992d77fd146d69eec2f244e6237d1a97796")
GITHUB_MCP_URL = "https://api.githubcopilot.com/mcp/x/repos/readonly"
GITHUB_MCP_ALL_URL = "https://api.githubcopilot.com/mcp/readonly"
GITHUB_READ_TOOLS = ("get_file_contents", "list_commits", "search_repositories")

# Plak's own bind/admission refusals. A tool row or image row that does not
# serve the route is the product's statement, reported as `refused`.
ROW_REFUSALS = (
    re.compile(r"\b(\S+) is not yet qualified on ([^\s;]+)"),
    re.compile(r"\b(\S+) is not available on ([^\s;]+)"),
    re.compile(r"route \S+ model \S+ does not accept image input"),
)

# Model-visible names per hosted retrieval tool (spec 02, tool vocabulary).
RETRIEVAL_NAMES = {
    "web_search": ("web_search", "WebSearch"),
    "web_fetch": ("web_fetch", "WebFetch"),
    "x_search": ("x_search",),
}


# --- pure oracle logic (covered by tests/tooling/test_live_features.py) ------


def row_refusal(text: str) -> str | None:
    """`refused: route row unsupported (<line>)` when Plak refused the row."""
    flat = " ".join(text.split())
    for pattern in ROW_REFUSALS:
        match = pattern.search(flat)
        if match:
            return f"refused: route row unsupported ({match.group(0)})"
    return None


def names_animal(answer: str, animal: str = "cat") -> bool:
    """The answer names the animal as a word (`cat`, `cats`, `kitten`)."""
    words = re.findall(r"[a-z]+", answer.lower())
    accepted = {animal, f"{animal}s"} | ({"kitten", "kitty", "feline"} if animal == "cat" else set())
    return any(word in accepted for word in words)


def image_magic(data: bytes) -> str | None:
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return "png"
    if data.startswith(b"\xff\xd8\xff"):
        return "jpeg"
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "webp"
    if data[:6] in (b"GIF87a", b"GIF89a"):
        return "gif"
    return None


def json_strings(value: object):
    """Every string inside a decoded JSON value, depth first."""
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for item in value.values():
            yield from json_strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from json_strings(item)


def _data_url_bytes(url: str) -> bytes | None:
    match = re.match(r"data:image/[a-z+.-]+;base64,(.*)", url, re.S)
    if not match:
        return None
    try:
        return base64.b64decode(match.group(1), validate=False)
    except ValueError:
        return None


def request_images(body: object) -> list[bytes]:
    """Image payloads a provider request carries, whatever its protocol.

    Messages: `{type: image, source: {type: base64, data}}`. Responses:
    `{type: input_image, image_url: "data:..."}`. Chat: `{type: image_url,
    image_url: {url: "data:..."}}`.
    """
    found: list[bytes] = []

    def walk(value: object) -> None:
        if isinstance(value, list):
            for item in value:
                walk(item)
            return
        if not isinstance(value, dict):
            return
        kind = value.get("type")
        payload: bytes | None = None
        if kind == "image" and isinstance(value.get("source"), dict):
            source = value["source"]
            if source.get("type") == "base64" and isinstance(source.get("data"), str):
                try:
                    payload = base64.b64decode(source["data"])
                except ValueError:
                    payload = None
        elif kind == "input_image" and isinstance(value.get("image_url"), str):
            payload = _data_url_bytes(value["image_url"])
        elif kind == "image_url":
            inner = value.get("image_url")
            url = inner.get("url") if isinstance(inner, dict) else inner
            if isinstance(url, str):
                payload = _data_url_bytes(url)
        if payload and image_magic(payload):
            found.append(payload)
            return
        for item in value.values():
            walk(item)

    walk(body)
    return found


def tool_calls(records: list[dict], names: tuple[str, ...] | str) -> list[dict]:
    """`tool.started` records whose canonical or exposed name matches.

    A name ending in `*` is a prefix; otherwise the match is exact or, for MCP
    tools, the `mcp__<server>__<tool>` suffix after the last `__`.
    """
    wanted = (names,) if isinstance(names, str) else names
    calls = []
    for record in records:
        if record.get("kind") != "tool.started":
            continue
        seen = {str(record.get("name", "")), str(record.get("exposed_name", ""))} - {""}
        for name in seen:
            short = name.rsplit("__", 1)[-1]
            if any(name == want or short == want
                   or (want.endswith("*") and name.startswith(want[:-1]))
                   for want in wanted):
                calls.append(record)
                break
    return calls


def answer_text(out: str, records: list[dict]) -> str:
    """Stdout (the last turn's answer) plus every logged assistant text."""
    lines = [line for line in out.splitlines() if not line.startswith("session_id=")]
    texts = [str(r.get("text", "")) for r in records if r.get("kind") == "turn.assistant"]
    return "\n".join([*lines, *texts])


_URL = re.compile(r"https?://[^\s<>\"')\]]+")


def source_urls(text: str) -> list[str]:
    """URLs listed after a `Sources:` line of a retrieval result."""
    urls: list[str] = []
    for block in text.split("Sources:")[1:]:
        for line in block.splitlines()[1:] if block.startswith("\n") else block.splitlines():
            line = line.strip()
            if not line:
                continue
            if not line.startswith("- "):
                break
            urls += [url.rstrip(".,;") for url in _URL.findall(line)]
    return list(dict.fromkeys(urls))


def marker_line(text: str) -> str:
    """The one SHA-256 the marker file records; a changed fixture is a harness error."""
    found = sorted(set(SHA256_HEX.findall(text)))
    if len(found) != 1:
        raise ScenarioFailure(HARNESS, f"{MARKER_PATH} records {len(found)} SHA-256 values, not 1")
    return found[0]


def x_status_ids(urls: list[str]) -> set[str]:
    return {match.group(1) for url in urls
            for match in [re.search(r"(?:x|twitter)\.com/[^/]+/status(?:es)?/(\d+)", url)]
            if match}


RUST_VERSION = re.compile(r"\b1\.(\d{2,3})\.(\d+)\b")


def rust_version(answer: str) -> str | None:
    match = RUST_VERSION.search(answer)
    return match.group(0) if match else None


# --- environment helpers -----------------------------------------------------


def code_mode(route: live.Route) -> bool:
    """Codex and the OpenAI API run native Code Mode by default."""
    return route.account in ("codex", "openai")


def capture_requests(segments: list[Path]) -> list[object]:
    bodies = []
    for segment in segments:
        for path in sorted(segment.glob("*.request")):
            try:
                bodies.append(json.loads(path.read_bytes()))
            except (OSError, ValueError):
                continue
    return bodies


def session_log(env: live.Env, home: Path, repo: Path, session: str) -> list[dict]:
    """`plak session log <id> --json`: the session's narrative, unchanged."""
    child = {key: value for key, value in os.environ.items()
             if key not in ("PLAK_TEST_FAULTS", "PLAK_TEST_EXCHANGE_CAPTURE_DIR")}
    child["PLAK_HOME"] = str(home)
    result = subprocess.run(
        [str(env.binary), "session", "log", session, "--json", "--tail", "100000"],
        cwd=repo, env=child, capture_output=True, text=True, timeout=60)
    if result.returncode != 0:
        raise ScenarioFailure(HARNESS, "plak session log --json failed: "
                                       + " ".join(result.stderr.split())[:200])
    records = []
    for line in result.stdout.splitlines():
        try:
            record = json.loads(line)
        except ValueError:
            continue
        if isinstance(record, dict):
            records.append(record)
    return records


def http_get(url: str, timeout: float = 30, limit: int = 8_388_608) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": BROWSER_UA})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return response.read(limit)


def cat_image(artifacts: Path) -> Path:
    """The cat photo fixture: `PLAK_LIVE_CAT_IMAGE`, or a cached download."""
    override = os.environ.get("PLAK_LIVE_CAT_IMAGE")
    path = Path(override) if override else live.ROOT / "target/live/assets/Cat03.jpg"
    if not path.is_file():
        path.parent.mkdir(parents=True, exist_ok=True)
        try:
            data = http_get(CAT_URL, timeout=60)
        except (OSError, urllib.error.URLError) as error:
            raise ScenarioFailure(HARNESS, f"cat photo download failed: {error}") from error
        path.write_bytes(data)
    data = path.read_bytes()
    if image_magic(data) is None or len(data) > IMAGE_LIMIT:
        raise ScenarioFailure(HARNESS, f"{path} is not an image under the 3.75 MiB cap")
    return path


def public_revision() -> str:
    """The newest first-parent ancestor of HEAD that GitHub serves.

    `PLAK_LIVE_PUBLIC_REV` pins one. A lane checkout of an unpushed commit
    falls back to the public commit it is built on.
    """
    pinned = os.environ.get("PLAK_LIVE_PUBLIC_REV")
    if pinned:
        return _git("rev-parse", pinned)
    for rev in _git("rev-list", "--first-parent", "--max-count=40", "HEAD").split():
        try:
            http_get(raw_url(rev), timeout=15, limit=1)
            return rev
        except (OSError, urllib.error.URLError):
            continue
    raise ScenarioFailure(HARNESS, "no recent ancestor of HEAD is served by GitHub; "
                                   "set PLAK_LIVE_PUBLIC_REV")


def raw_url(rev: str) -> str:
    return f"https://raw.githubusercontent.com/{PUBLIC_REPO}/{rev}/{MARKER_PATH}"


def _git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=live.ROOT, check=True,
                          capture_output=True, text=True).stdout.strip()


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def everything_server() -> Path:
    """The pinned reference server, installed once under target/live/."""
    prefix = live.ROOT / "target/live/assets" / f"server-everything-{EVERYTHING_VERSION}"
    entry = prefix / "node_modules" / EVERYTHING_PACKAGE / "dist" / "index.js"
    if not entry.is_file():
        prefix.mkdir(parents=True, exist_ok=True)
        result = subprocess.run(
            ["npm", "install", "--silent", "--no-audit", "--no-fund", "--prefix", str(prefix),
             f"{EVERYTHING_PACKAGE}@{EVERYTHING_VERSION}"],
            capture_output=True, text=True, timeout=300)
        if result.returncode != 0 or not entry.is_file():
            raise ScenarioFailure(HARNESS, "npm install of the reference MCP server failed: "
                                           + " ".join(result.stderr.split())[:200])
    return entry


def toml_string(value: str) -> str:
    return json.dumps(value)


def toml_array(values) -> str:
    return "[" + ", ".join(toml_string(value) for value in values) + "]"


# --- scenarios -----------------------------------------------------------------


class Features:
    def __init__(self, route: live.Route, env: live.Env):
        self.route, self.env = route, env

    # One `plak run` on a private home whose root template lists `tools`.
    def run(self, name: str, prompt: str, tools: list[str], *, config: str = "",
            images: list[Path] = (), route: live.Route | None = None,
            stage: str = "turn", timeout: float = 600,
            before=None) -> tuple[str, list[dict], list[Path], Path, str]:
        route = route or self.route
        base = self.env.artifacts / name
        repo = base / "repo"
        if not repo.exists():
            live.git_repo(repo, {"README.md": f"{name} scenario workspace\n"})
        home = base / ("home" if stage == "turn" else f"home-{stage}")
        live.write_home(home, route, self.env.home)
        set_tools(home, tools)
        if config:
            with (home / "config.toml").open("a") as file:
                file.write("\n" + config)
        if before:
            before(home)
        segment_start = len(self.env.capture_segments)
        log = base / f"{stage}.log"
        args = [*route.flags(), "--print-session-id"]
        for image in images:
            args += ["--image", str(image)]
        code, out = live.run_cli(self.env, repo, [*args, "--", prompt], log,
                                 timeout=timeout, home=home)
        segments = self.env.capture_segments[segment_start:]
        stderr = log.read_text(errors="replace").split("--- stderr\n", 1)[-1]
        session = next((line.split("=", 1)[1] for line in out.splitlines()
                        if line.startswith("session_id=")), None)
        if code != 0:
            refused = row_refusal(stderr)
            if refused:
                raise ScenarioFailure(REFUSED, refused)
            live.classify_cli_failure(code, stderr, f"{stage} ({route.name})", segments=segments)
        if not session:
            raise ScenarioFailure(HARNESS, f"{stage} exited 0 without a session id")
        records = session_log(self.env, home, repo, session)
        import smoke
        terminals = smoke.durable_terminals(records)
        if not terminals:
            raise ScenarioFailure(HARNESS, f"{stage} session {session}: no durable terminal "
                                           "in the session log")
        smoke.require_durable_completed(terminals[-1], f"{stage} session {session}",
                                        segments=segments, home=home)
        return out, records, segments, repo, session

    # --- image input ----------------------------------------------------------

    def image_input(self) -> str:
        image = cat_image(self.env.artifacts)
        out, records, segments, _, session = self.run(
            "image_input", "What animal is in this image? Answer with one word.", [],
            images=[image])
        sent = _image_sent(segments, image.read_bytes())
        answer = answer_text(out, records)
        if not names_animal(answer):
            raise ScenarioFailure(MODEL, f"session {session}: answer does not name the cat")
        return f"session {session}: answer names the cat; request carried {sent}"

    # --- Plak-run fetch ----------------------------------------------------------

    def fetch_url(self) -> str:
        rev, marker = _marker()
        out, records, _, _, session = self.run(
            "fetch_url",
            f"Use the fetch_url tool to fetch {raw_url(rev)} . The file records exactly one "
            "SHA-256 value (64 hexadecimal characters). Reply with that value only.",
            ["fetch_url"])
        calls = [call for call in tool_calls(records, "fetch_url")
                 if rev in json.dumps(call)]
        if not calls:
            raise ScenarioFailure(MODEL, f"session {session}: no fetch_url call of the raw URL "
                                         "in the session log")
        if marker not in answer_text(out, records):
            raise ScenarioFailure(MODEL, f"session {session}: answer lacks the fetched SHA-256")
        return f"session {session}: fetch_url of {rev[:12]} raw file; marker in the answer"

    # --- MCP: the reference server ------------------------------------------------

    def mcp_stdio(self) -> str:
        return self._mcp_everything("mcp_stdio", "stdio")

    def mcp_http(self) -> str:
        return self._mcp_everything("mcp_http", "http")

    def mcp_everything_all(self) -> str:
        """Every tool the reference server lists (no allowlist), over stdio."""
        return self._mcp_everything("mcp_everything_all", "stdio", wildcard=True)

    def _mcp_everything(self, name: str, transport: str, wildcard: bool = False) -> str:
        entry = everything_server()
        node = shutil.which("node")
        if not node:
            raise ScenarioFailure(HARNESS, "node is not on PATH")
        base = self.env.artifacts / name
        base.mkdir(parents=True, exist_ok=True)
        token = f"mcp-{secrets.token_hex(8)}"
        a, b = 1000 + secrets.randbelow(90000), 1000 + secrets.randbelow(90000)
        server = None
        tools = ["get-sum", "get-env"]
        allowlist = "" if wildcard else f"tools = {toml_array(tools)}\n"
        if transport == "stdio":
            wrapper = base / "server.sh"
            wrapper.write_text(
                "#!/bin/sh\n"
                f"tee -a {_sh(base / 'server-stdin.jsonl')} | "
                f"{_sh(node)} {_sh(entry)} stdio | tee -a {_sh(base / 'server-stdout.jsonl')}\n")
            wrapper.chmod(0o700)
            config = ("[mcp_servers.everything]\ntransport = \"stdio\"\n"
                      f"command = {toml_string(str(wrapper))}\n"
                      f"env = {{ PLAK_LIVE_MCP_TOKEN = {toml_string(token)} }}\n"
                      f"{allowlist}startup_seconds = 60\n")
        else:
            port = free_port()
            server_log = (base / "server.log").open("w")
            server = subprocess.Popen(
                [node, str(entry), "streamableHttp"], stdout=server_log,
                stderr=subprocess.STDOUT, start_new_session=True,
                env={"PATH": os.environ.get("PATH", ""), "HOME": str(base),
                     "PORT": str(port), "PLAK_LIVE_MCP_TOKEN": token})
            if not _wait_for(lambda: "listening on port" in (base / "server.log").read_text(), 30):
                server.terminate()
                raise ScenarioFailure(HARNESS, "reference MCP server did not start")
            config = ("[mcp_servers.everything]\ntransport = \"http\"\n"
                      f"url = \"http://127.0.0.1:{port}/mcp\"\n{allowlist}")
        cell = (" Make both calls from inside one exec cell (Code Mode), not as direct "
                "tool calls." if code_mode(self.route) else "")
        prompt = (f"Use the MCP server named everything. Call its get-sum tool with a={a} and "
                  f"b={b}, then call its get-env tool.{cell} Reply on one line with the sum "
                  "returned by get-sum and the value of PLAK_LIVE_MCP_TOKEN returned by get-env.")
        try:
            out, records, segments, _, session = self.run(name, prompt, ["mcp:everything"],
                                                          config=config)
        finally:
            if server is not None:
                server.terminate()
                server.wait(10)
        requests = capture_requests(segments)
        detail = f"session {session}"
        require_offered(requests, "everything", tools, session)
        answer = answer_text(out, records)
        if not tool_calls(records, "get-sum") or not tool_calls(records, "get-env"):
            raise ScenarioFailure(MODEL, f"{detail}: session log lacks the get-sum/get-env calls")
        if str(a + b) not in answer:
            raise ScenarioFailure(MODEL, f"{detail}: answer lacks the sum {a + b}")
        if token not in answer:
            raise ScenarioFailure(MODEL, f"{detail}: answer lacks the server-only token")
        if transport == "stdio":
            calls = _jsonrpc_tool_calls(base / "server-stdin.jsonl")
            if not any(call.get("name") == "get-sum"
                       and call.get("arguments") == {"a": a, "b": b} for call in calls):
                raise ScenarioFailure(HARNESS, f"{detail}: server stdin has no get-sum "
                                               f"tools/call with a={a}, b={b}")
            detail += "; server stdin shows tools/call get-sum with the arguments"
        else:
            posts = (base / "server.log").read_text().count("Received MCP POST request")
            detail += f"; server saw {posts} POSTs and returned its own token"
        if code_mode(self.route):
            if not any(name in source for source in cell_sources(requests)
                       for name in ("mcp__everything__get-sum", "mcp__everything__get_sum")):
                raise ScenarioFailure(MODEL, f"{detail}: no Code Mode cell called get-sum")
            detail += "; called from a Code Mode cell"
        return detail

    # --- MCP: the official GitHub server ---------------------------------------------

    def mcp_github_stdio(self) -> str:
        if not shutil.which("docker"):
            raise ScenarioFailure(HARNESS, "docker is not on PATH")
        config = ("[mcp_servers.github]\ntransport = \"stdio\"\ncommand = \"docker\"\n"
                  "args = " + toml_array([
                      "run", "-i", "--rm", "-e", "GITHUB_PERSONAL_ACCESS_TOKEN",
                      GITHUB_MCP_IMAGE, "stdio", "--read-only",
                      "--tools", ",".join(GITHUB_READ_TOOLS)]) + "\n"
                  "secret_env = { GITHUB_PERSONAL_ACCESS_TOKEN = \"github\" }\n"
                  f"tools = {toml_array(GITHUB_READ_TOOLS)}\nstartup_seconds = 120\n")
        return self._mcp_github("mcp_github_stdio", config)

    def mcp_github_all_stdio(self) -> str:
        """The server's whole read-only default tool set, no allowlist anywhere."""
        if not shutil.which("docker"):
            raise ScenarioFailure(HARNESS, "docker is not on PATH")
        config = ("[mcp_servers.github]\ntransport = \"stdio\"\ncommand = \"docker\"\n"
                  "args = " + toml_array([
                      "run", "-i", "--rm", "-e", "GITHUB_PERSONAL_ACCESS_TOKEN",
                      GITHUB_MCP_IMAGE, "stdio", "--read-only"]) + "\n"
                  "secret_env = { GITHUB_PERSONAL_ACCESS_TOKEN = \"github\" }\n"
                  "startup_seconds = 120\n")
        return self._mcp_github("mcp_github_all_stdio", config)

    def mcp_github_all_http(self) -> str:
        """The remote server's read-only endpoint over every toolset, no allowlist."""
        config = ("[mcp_servers.github]\ntransport = \"http\"\n"
                  f"url = {toml_string(GITHUB_MCP_ALL_URL)}\nbearer_secret = \"github\"\n"
                  "headers = { X-MCP-Readonly = \"true\" }\n")
        return self._mcp_github("mcp_github_all_http", config)

    def mcp_github_http(self) -> str:
        config = ("[mcp_servers.github]\ntransport = \"http\"\n"
                  f"url = {toml_string(GITHUB_MCP_URL)}\nbearer_secret = \"github\"\n"
                  "headers = { X-MCP-Readonly = \"true\" }\n"
                  f"tools = {toml_array(GITHUB_READ_TOOLS)}\n")
        return self._mcp_github("mcp_github_http", config)

    def _mcp_github(self, name: str, config: str) -> str:
        rev, marker = _marker()
        owner, repo_name = PUBLIC_REPO.split("/")
        cell = (" Make the call from inside an exec cell (Code Mode)."
                if code_mode(self.route) else "")
        prompt = (f"Use the github MCP server's get_file_contents tool with owner {owner}, "
                  f"repo {repo_name}, path {MARKER_PATH} and ref {rev}.{cell} The file records "
                  "exactly one SHA-256 value (64 hexadecimal characters). Reply with that "
                  "value only.")
        out, records, segments, _, session = self.run(
            name, prompt, ["mcp:github"], config=config, before=self._github_secret)
        detail = f"session {session}"
        require_offered(capture_requests(segments), "github", ["get_file_contents"], session)
        if not tool_calls(records, "get_file_contents"):
            raise ScenarioFailure(MODEL, f"{detail}: session log has no get_file_contents call")
        if marker not in answer_text(out, records):
            raise ScenarioFailure(MODEL, f"{detail}: answer lacks the file's SHA-256")
        return f"{detail}: get_file_contents at {rev[:12]}; marker in the answer"

    def _github_secret(self, home: Path) -> None:
        """`gh auth token` into this private home only; never printed or written elsewhere."""
        if not shutil.which("gh"):
            raise ScenarioFailure(HARNESS, "gh is not on PATH")
        token = subprocess.run(["gh", "auth", "token"], capture_output=True, timeout=30)
        if token.returncode != 0 or not token.stdout.strip():
            raise ScenarioFailure(HARNESS, "gh auth token returned no token")
        child = {key: value for key, value in os.environ.items()
                 if key not in ("PLAK_TEST_FAULTS", "PLAK_TEST_EXCHANGE_CAPTURE_DIR")}
        child["PLAK_HOME"] = str(home)
        result = subprocess.run([str(self.env.binary), "secret", "set", "github"],
                                input=token.stdout.strip() + b"\n", env=child,
                                capture_output=True, timeout=30)
        if result.returncode != 0:
            raise ScenarioFailure(HARNESS, "plak secret set github failed (exit "
                                           f"{result.returncode})")

    # --- hosted retrieval ---------------------------------------------------------------

    def web_search(self) -> str:
        out, records, segments, _, session = self.run(
            "web_search",
            "Use web search to find the latest stable release of the Rust programming "
            "language. Reply with only its version number (like 1.NN.N).", ["web_search"])
        urls = self._retrieval_sources("web_search", records, segments, session)
        version = rust_version(answer_text(out, records))
        if not version:
            raise ScenarioFailure(MODEL, f"session {session}: answer has no Rust version")
        page = _fact_on_pages(version, urls)
        if not page:
            raise ScenarioFailure(MODEL, f"session {session}: {version} is on no readable "
                                         f"cited page ({len(urls)} sources)")
        return f"session {session}: web_search, {len(urls)} sources; {version} on {page}"

    def web_fetch(self) -> str:
        rev, marker = _marker()
        out, records, segments, _, session = self.run(
            "web_fetch",
            f"Use web fetch to read {raw_url(rev)} . The file records exactly one SHA-256 "
            "value (64 hexadecimal characters). Reply with that value only.", ["web_fetch"])
        self._retrieval_sources("web_fetch", records, segments, session, require_sources=False)
        if marker not in answer_text(out, records):
            raise ScenarioFailure(MODEL, f"session {session}: answer lacks the fetched SHA-256")
        return f"session {session}: web_fetch of the {rev[:12]} raw file; marker in the answer"

    def x_search(self) -> str:
        out, records, segments, _, session = self.run(
            "x_search",
            "Use X search to find one recent post by @rustlang. Reply with that post's URL "
            "only.", ["x_search"])
        urls = self._retrieval_sources("x_search", records, segments, session)
        ids = x_status_ids(urls)
        if not ids:
            raise ScenarioFailure(MODEL, f"session {session}: no X post among the sources")
        answered = x_status_ids(_URL.findall(answer_text(out, records)))
        if not answered & ids:
            raise ScenarioFailure(MODEL, f"session {session}: answer names no cited X post")
        return f"session {session}: x_search, {len(ids)} cited posts; answer names one"

    def _retrieval_sources(self, tool: str, records: list[dict], segments: list[Path],
                           session: str, require_sources: bool = True) -> list[str]:
        if not tool_calls(records, RETRIEVAL_NAMES[tool]):
            raise ScenarioFailure(MODEL, f"session {session}: no {tool} call in the session log")
        texts = [str(r.get("detail", "")) for r in records if r.get("kind") == "tool.completed"]
        texts += [text for body in capture_requests(segments) for text in json_strings(body)
                  if "Sources:" in text]
        urls = source_urls("\n".join(texts))
        if require_sources and not urls:
            raise ScenarioFailure(MODEL, f"session {session}: {tool} result lists no sources")
        return urls

    # --- image generation round trip ------------------------------------------------------

    def image_gen_roundtrip(self) -> str:
        out, records, _, repo, session = self.run(
            "image_gen_roundtrip",
            "Use the image_gen tool to generate a photo of a cat and save it in this "
            "workspace. Reply with the saved file name only.", ["image_gen"])
        if not tool_calls(records, "image_gen"):
            raise ScenarioFailure(MODEL, f"session {session}: no image_gen call in the log")
        images = [path for path in sorted(repo.rglob("*"))
                  if path.is_file() and ".git" not in path.parts
                  and image_magic(path.read_bytes()[:16]) in ("png", "jpeg")]
        if not images:
            raise ScenarioFailure(MODEL, f"session {session}: no PNG/JPEG in the workspace")
        vision = live.ROUTES[os.environ.get("PLAK_LIVE_VISION_ROUTE", "claude-haiku55-low")]
        out2, records2, segments2, _, session2 = self.run(
            "image_gen_roundtrip", "What animal is in this image? Answer with one word.", [],
            images=[images[0]], route=vision, stage="vision")
        sent = _image_sent(segments2, images[0].read_bytes())
        if not names_animal(answer_text(out2, records2)):
            raise ScenarioFailure(MODEL, f"vision session {session2} ({vision.name}) does not "
                                         "name a cat")
        return (f"session {session}: {images[0].name} generated; vision session {session2} "
                f"({vision.name}, {sent}) names the cat")


def set_tools(home: Path, tools: list[str]) -> None:
    """List `tools` on the root template `live.write_home` wrote."""
    if not tools:
        return
    path = home / "agents" / "root-agent.toml"
    text = path.read_text()
    if "\ntools = " in text:
        raise ScenarioFailure(HARNESS, "root template already lists tools")
    path.write_text(f"{text}tools = {toml_array(tools)}\n")


def _sh(value: object) -> str:
    return "'" + str(value).replace("'", "'\\''") + "'"


def _wait_for(predicate, seconds: float) -> bool:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            if predicate():
                return True
        except OSError:
            pass
        time.sleep(0.25)
    return False


def _marker() -> tuple[str, str]:
    rev = public_revision()
    return rev, marker_line(_git("show", f"{rev}:{MARKER_PATH}"))


def _image_sent(segments: list[Path], original: bytes) -> str:
    """The capture's evidence that the image reached the provider."""
    images = [image for body in capture_requests(segments) for image in request_images(body)]
    if not images:
        raise ScenarioFailure(HARNESS, "no captured request carries an image block")
    digest = hashlib.sha256(original).hexdigest()
    if any(hashlib.sha256(image).hexdigest() == digest for image in images):
        return "the exact image bytes"
    return f"a re-encoded {image_magic(images[0])} image"


def _jsonrpc_tool_calls(path: Path) -> list[dict]:
    calls = []
    if not path.is_file():
        return calls
    for line in path.read_text(errors="replace").splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if isinstance(message, dict) and message.get("method") == "tools/call":
            params = message.get("params")
            if isinstance(params, dict):
                calls.append(params)
    return calls


def mcp_offered(requests: list[object], server: str, tool: str) -> bool:
    """The model's first request names `mcp__<server>__<tool>` (direct tool or cell catalog)."""
    return bool(requests) and f"mcp__{server}__{tool}" in json.dumps(requests[0])


def cell_sources(requests: list[object]) -> list[str]:
    """Code Mode `exec` cell sources the model sent, from the last request's input."""
    if not requests or not isinstance(requests[-1], dict):
        return []
    items = requests[-1].get("input")
    return [item["input"] for item in items if isinstance(items, list)
            and isinstance(item, dict) and item.get("type") == "custom_tool_call"
            and isinstance(item.get("input"), str)] if isinstance(items, list) else []


def require_offered(requests: list[object], server: str, tools, session: str) -> None:
    """A configured, allowlisted MCP tool Plak left out of the catalog is Plak's failure."""
    missing = [tool for tool in tools if not mcp_offered(requests, server, tool)]
    if missing:
        raise ScenarioFailure(HARNESS, f"session {session}: Plak did not offer "
                                       f"{', '.join(f'mcp__{server}__{t}' for t in missing)} "
                                       "to the model")


def _fact_on_pages(fact: str, urls: list[str]) -> str | None:
    for url in urls[:8]:
        try:
            page = http_get(url, timeout=20, limit=4_194_304).decode("utf-8", "replace")
        except (OSError, urllib.error.URLError, ValueError):
            continue
        if fact in page:
            return url
    return None


def play(route: live.Route, env: live.Env, name: str) -> str:
    if name not in dict(CATALOG):
        raise ScenarioFailure(HARNESS, f"unknown feature scenario {name}")
    return getattr(Features(route, env), name)()
