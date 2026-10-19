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
import tomllib
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
    ("mcp_stdio_py", "cli"),
    ("mcp_github_stdio", "cli"),
    ("mcp_github_http", "cli"),
    ("mcp_everything_all", "cli"),
    ("mcp_github_all_stdio", "cli"),
    ("mcp_github_all_http", "cli"),
    ("web_search", "cli"),
    ("web_fetch", "cli"),
    ("x_search", "cli"),
    ("image_gen_roundtrip", "cli"),
    ("child_own_tools", "cli"),
    ("profile_researcher", "cli"),
    ("multi_roles", "cli"),
    ("worker_spawns_researcher", "cli"),
    ("wide_fanout", "cli"),
    ("web_search_401", "cli"),
    ("image_gen_401", "cli"),
)

# One injected 401 on the first subscription side request of each kind. On
# SuperGrok search shares the model's path, so its second request is the
# side request (the first is the round that calls web_search).
SIDE_REQUEST_401 = {
    ("codex", "web_search"): "host=chatgpt.com,path=/backend-api/codex/alpha/search,nth=1,fault=401",
    ("codex", "image_gen"): "host=chatgpt.com,path=/backend-api/codex/images/,nth=1,fault=401",
    ("grok", "web_search"): "host=cli-chat-proxy.grok.com,path=/v1/responses,nth=2,fault=401",
    ("grok", "image_gen"): "host=cli-chat-proxy.grok.com,path=/v1/images/,nth=1,fault=401",
}
_GENERATION = re.compile(r"generation (\d+)")

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


def row_refusal(text: str, *, records: list[dict] | None = None) -> str | None:
    """`refused: route row unsupported (<line>)` when Plak refused the row."""
    if any(record.get("kind") == "turn.lifecycle" for record in records or ()):
        return None
    flat = " ".join(text.split())
    if re.search(r"recovery_required|OutcomeUnknown|turn requires recovery|turn failed", flat):
        return None
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
    """Every string inside a decoded JSON value, depth first.

    A string that is itself a JSON document (a Claude-vocabulary tool result
    is `{"result": "..."}` text) also yields the strings inside it.
    """
    if isinstance(value, str):
        yield value
        if value[:1] in ("{", "["):
            try:
                inner = json.loads(value)
            except ValueError:
                return
            yield from json_strings(inner)
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


# An older release whose date every index has had for months: the scenario
# tests retrieval, not how fresh a provider's index is.
RUST_DATED_RELEASE = "1.95.0"
RUST_RELEASE_CHANNEL = f"https://static.rust-lang.org/dist/channel-rust-{RUST_DATED_RELEASE}.toml"
ISO_DATE = re.compile(r"\b(\d{4})-(\d{2})-(\d{2})\b")
MONTHS = ("January", "February", "March", "April", "May", "June", "July", "August",
          "September", "October", "November", "December")


def channel_release_date(manifest: bytes) -> str | None:
    """The `YYYY-MM-DD` date a release channel manifest was published on."""
    date = tomllib.loads(manifest.decode("utf-8")).get("date")
    return date if isinstance(date, str) and ISO_DATE.fullmatch(date) else None


def date_forms(iso: str) -> list[str]:
    """The spellings a page may give one date in: ISO, `April 16, 2026`, `16 April 2026`."""
    year, month, day = (int(part) for part in iso.split("-"))
    name = MONTHS[month - 1]
    return [iso, f"{name} {day}, {year}", f"{day} {name} {year}", f"{name[:3]} {day}, {year}"]


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
            stage: str = "turn", timeout: float = 600, composition: str | None = None,
            faults: str | None = None, before=None,
            profile: str | None = None) -> tuple[str, list[dict], list[Path], Path, str]:
        route = route or self.route
        base = self.env.artifacts / name
        repo = base / "repo"
        if not repo.exists():
            live.git_repo(repo, {"README.md": f"{name} scenario workspace\n"})
        home = base / ("home" if stage == "turn" else f"home-{stage}")
        if profile:
            write_profile_home(home, profile)
        else:
            live.write_home(home, route, self.env.home)
            set_tools(home, tools)
        if composition:
            live.pin_composition(home, composition)
        if config:
            with (home / "config.toml").open("a") as file:
                file.write("\n" + config)
        if before:
            before(home)
        segment_start = len(self.env.capture_segments)
        log = base / f"{stage}.log"
        # A profile's root runs on its own root template's exact tuple.
        flags = profile_root_flags(profile) if profile else route.flags()
        args = [*flags, "--print-session-id"]
        if composition:
            args += ["--composition", composition]
        for image in images:
            args += ["--image", str(image)]
        code, out = live.run_cli(self.env, repo, [*args, "--", prompt], log,
                                 timeout=timeout, home=home, faults=faults)
        segments = self.env.capture_segments[segment_start:]
        stderr = log.read_text(errors="replace").split("--- stderr\n", 1)[-1]
        session = next((line.split("=", 1)[1] for line in out.splitlines()
                        if line.startswith("session_id=")), None)
        records = session_log(self.env, home, repo, session) if session else []
        if code != 0:
            refused = row_refusal(stderr, records=records)
            if refused:
                raise ScenarioFailure(REFUSED, refused)
            live.classify_cli_failure(code, stderr, f"{stage} ({route.name})", segments=segments)
        if not session:
            raise ScenarioFailure(HARNESS, f"{stage} exited 0 without a session id")
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

    def mcp_stdio_py(self) -> str:
        """The stdio reference server from a Python Code Mode cell: `get-sum` is
        not a Python identifier, so the cell must reach it as `get_sum`."""
        return self._mcp_everything("mcp_stdio_py", "stdio", composition="py-ptc")

    def mcp_everything_all(self) -> str:
        """Every tool the reference server lists (no allowlist), over stdio."""
        return self._mcp_everything("mcp_everything_all", "stdio", wildcard=True)

    def _mcp_everything(self, name: str, transport: str, wildcard: bool = False,
                        composition: str | None = None) -> str:
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
        python = composition == "py-ptc"
        cell = (" Make both calls from inside one exec_py Python cell." if python else
                " Make both calls from inside one exec cell (Code Mode), not as direct "
                "tool calls." if code_mode(self.route) else "")
        prompt = (f"Use the MCP server named everything. Call its get-sum tool with a={a} and "
                  f"b={b}, then call its get-env tool.{cell} Reply on one line with the sum "
                  "returned by get-sum and the value of PLAK_LIVE_MCP_TOKEN returned by get-env.")
        try:
            out, records, segments, _, session = self.run(name, prompt, ["mcp:everything"],
                                                          config=config,
                                                          composition=composition)
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
        if python:
            if not tool_calls(records, "exec_py"):
                raise ScenarioFailure(MODEL, f"{detail}: session log has no exec_py cell")
            if not any("mcp__everything__get_sum(" in json.dumps(request)
                       for request in requests):
                raise ScenarioFailure(MODEL, f"{detail}: no Python cell called "
                                             "mcp__everything__get_sum")
            detail += "; called as mcp__everything__get_sum from an exec_py cell"
        elif code_mode(self.route):
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
            f"Use web search to find the date Rust {RUST_DATED_RELEASE} was released as "
            "stable. Reply with only that date as YYYY-MM-DD.", ["web_search"])
        urls = self._retrieval_sources("web_search", records, segments, session)
        answered = ISO_DATE.search(answer_text(out, records))
        if not answered:
            raise ScenarioFailure(MODEL, f"session {session}: answer has no YYYY-MM-DD date")
        try:
            released = channel_release_date(http_get(RUST_RELEASE_CHANNEL, timeout=30))
        except (OSError, urllib.error.URLError, ValueError) as error:
            raise ScenarioFailure(HARNESS, f"release channel manifest unreadable: {error}")
        if not released:
            raise ScenarioFailure(HARNESS, "release channel manifest names no date")
        if answered.group(0) != released:
            raise ScenarioFailure(MODEL, f"session {session}: answered {answered.group(0)}; "
                                         f"Rust {RUST_DATED_RELEASE} shipped {released}")
        page = next((page for form in date_forms(released)
                     for page in [_fact_on_pages(form, urls)] if page), None)
        if not page:
            raise ScenarioFailure(MODEL, f"session {session}: {released} is on no readable "
                                         f"cited page ({len(urls)} sources)")
        return (f"session {session}: web_search, {len(urls)} sources; Rust "
                f"{RUST_DATED_RELEASE} {released} on {page}")

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
        # The completed record's detail is a bounded preview that may cut the
        # `Sources:` heading; the sent request carries the whole result.
        details = [str(r.get("detail", "")) for r in records if r.get("kind") == "tool.completed"]
        urls = source_urls("\n".join(details))
        if not urls:
            urls = source_urls("\n".join(
                text for body in capture_requests(segments) for text in json_strings(body)
                if "Sources:" in text))
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


    # --- a 401 on a subscription side request ------------------------------------------

    def credential_generation(self) -> int:
        """The stored login's generation, from `plak doctor`; a refresh bumps it."""
        child = {key: value for key, value in os.environ.items()
                 if key not in ("PLAK_TEST_EXCHANGE_CAPTURE_DIR", "PLAK_TEST_FAULTS")}
        doctor_home = self.env.artifacts / "doctor-home"
        doctor_home.mkdir(mode=0o700, exist_ok=True)
        child["PLAK_HOME"] = str(doctor_home)
        result = subprocess.run([str(self.env.binary), "doctor"], env=child,
                                capture_output=True, text=True, timeout=120)
        for line in (result.stdout + result.stderr).splitlines():
            match = _GENERATION.search(line)
            if match and self.route.account in line.lower():
                return int(match.group(1))
        raise ScenarioFailure(HARNESS, f"plak doctor shows no {self.route.account} generation")

    def _side_request_401(self, tool: str) -> tuple[str, list[dict], list[Path], Path, str, str]:
        plan = SIDE_REQUEST_401.get((self.route.account, tool))
        if not plan:
            raise ScenarioFailure(REFUSED, f"no subscription side request for {tool} here")
        before = self.credential_generation()
        prompt = ("Use web search to find the date Rust 1.80.0 was released as stable. "
                  "Reply with only that date as YYYY-MM-DD." if tool == "web_search" else
                  "Use the image_gen tool to generate a photo of a cat and save it in this "
                  "workspace. Reply with the saved file name only.")
        out, records, segments, repo, session = self.run(f"{tool}_401", prompt, [tool],
                                                         faults=plan)
        after = self.credential_generation()
        self.refreshes = refresh_rows(self.env.artifacts / f"{tool}_401" / "home",
                                      self.route.account)
        if after <= before:
            raise ScenarioFailure(HARNESS, f"session {session}: login generation stayed "
                                           f"{before}; the 401 forced no refresh")
        # One refresh is two record writes: the claim stores the old payload as
        # outcome-unknown at G+1 before any token I/O, the success stores Ready
        # at G+2. Anything else is a second refresh.
        grants = sum(phase == "completed" for phase, _ in self.refreshes)
        if self.refreshes and after - before != 2 * grants:
            raise ScenarioFailure(HARNESS, f"session {session}: generation {before} -> {after} "
                                           f"for {grants} completed refresh(es)")
        return out, records, segments, repo, session, f"generation {before} -> {after}"

    def web_search_401(self) -> str:
        """An injected 401 on the search side request: one claimed refresh,
        one more request, and the search succeeds."""
        _, records, segments, _, session, refreshed = self._side_request_401("web_search")
        urls = self._retrieval_sources("web_search", records, segments, session)
        return (f"session {session}: 401 then {refreshed}; refresh telemetry "
                f"{self.refreshes}; web_search returned {len(urls)} sources")

    def image_gen_401(self) -> str:
        """An injected 401 on the images request: refresh, resend, image saved."""
        _, records, _, repo, session, refreshed = self._side_request_401("image_gen")
        failed = [r for r in records if r.get("kind") in ("tool.failed", "tool.rejected")]
        images = [path for path in sorted(repo.rglob("*"))
                  if path.is_file() and ".git" not in path.parts
                  and image_magic(path.read_bytes()[:16]) in ("png", "jpeg")]
        if not tool_calls(records, "image_gen") or not images:
            raise ScenarioFailure(HARNESS, f"session {session}: no image after the 401 "
                                           f"({len(failed)} failed tool records)")
        return f"session {session}: 401 then {refreshed}; {images[0].name} saved"

    # --- the child-tool bound after a root template edit ----------------------------

    def child_own_tools(self) -> str:
        """A child gets exactly its own template's tools: a root with no
        optional tools spawns a worker that lists `fetch_url`, and the worker
        is offered and runs it."""
        rev, marker = _marker()

        def arrange(home: Path) -> None:
            set_tools(home, ["fetch_url"], "worker")

        out, records, segments, _, session = self.run(
            "child_own_tools",
            "Spawn one worker (template worker, tier execute, workspace inherit) and tell it: "
            f"use fetch_url to read {raw_url(rev)} and reply with the one SHA-256 value "
            "(64 hexadecimal characters) the file records. Wait for the worker, then reply "
            "with that value only.",
            [], before=arrange, timeout=900)
        detail = f"session {session}"
        if not tool_calls(records, "spawn_agent"):
            raise ScenarioFailure(MODEL, f"{detail}: the model never called spawn_agent")
        requests = capture_requests(segments)
        root = [body for body in requests if offers_card(body, "worker")]
        child = [body for body in requests if not offers_card(body, "worker")]
        if any(offered(body, "fetch_url") for body in root):
            raise ScenarioFailure(HARNESS, f"{detail}: the root was offered fetch_url")
        if not child or not all(offered(body, "fetch_url") for body in child):
            raise ScenarioFailure(HARNESS, f"{detail}: the worker was not offered fetch_url")
        if marker not in answer_text(out, records):
            raise ScenarioFailure(MODEL, f"{detail}: answer lacks the fetched SHA-256")
        return (f"{detail}: root without optional tools spawned a worker listing fetch_url; "
                "the worker fetched the marker")

    # --- built-in profiles: the default role set ----------------------------------

    def _research(self, name: str, profile: str, *, spawner: str = "root",
                  config_edit=None) -> tuple[str, list[object], Path]:
        """The researcher web-searches and writes research/<topic>/report.md.

        `spawner = "worker"` asks the root to delegate to the worker, which
        spawns the researcher itself (needs max_depth 2)."""
        x = profile in ("grok-default", "multi-default")
        task = ("use web_search to find the date Rust " + RUST_DATED_RELEASE + " was released "
                "as stable" + (", and use x_search to find one recent post by @rustlang" if x
                               else "") +
                "; write research/rust-release/report.md with the findings and a Sources "
                "list of every URL used, then return the path")
        if spawner == "root":
            prompt = ("Spawn the researcher (template " + profile + "-researcher, tier mutate, "
                      "workspace inherit) with this task: " + task + ". Wait for it, then "
                      "reply with the report path only.")
        else:
            prompt = ("Spawn the worker (template " + profile + "-worker, tier mutate, "
                      "workspace inherit) and tell it: spawn the researcher (template " +
                      profile + "-researcher, tier mutate, workspace inherit) yourself with "
                      "this task: " + task + "; wait for it and return the path. Do not "
                      "spawn the researcher yourself. Wait for the worker, then reply with "
                      "the report path only.")
        out, records, segments, repo, session = self.run(
            name, prompt, [], profile=profile, before=config_edit, timeout=1500)
        detail = f"session {session}"
        requests = capture_requests(segments)
        researcher = profile_model(profile, "researcher")
        root_card = f"{profile}-worker"
        roots = [body for body in requests if offers_card(body, root_card)]
        if any(offered(body, "web_search") for body in roots):
            raise ScenarioFailure(HARNESS, f"{detail}: the root was offered web_search")
        child = [body for body in requests if request_model(body) == researcher
                 and offered(body, "web_search")]
        if not child:
            raise ScenarioFailure(MODEL, f"{detail}: no researcher ({researcher}) request "
                                         "offering web_search")
        urls = source_urls("\n".join(text for body in child for text in json_strings(body)
                                     if "Sources:" in text))
        if not urls:
            raise ScenarioFailure(MODEL, f"{detail}: no web_search result with sources")
        if x and not x_status_ids(urls):
            raise ScenarioFailure(MODEL, f"{detail}: no x_search post among the sources")
        reports = sorted((repo / "research").glob("*/report.md"))
        if not reports:
            raise ScenarioFailure(MODEL, f"{detail}: no research/<topic>/report.md")
        written = _URL.findall(reports[0].read_text(errors="replace"))
        if not written:
            raise ScenarioFailure(MODEL, f"{detail}: {reports[0].relative_to(repo)} cites no URL")
        root_spawns = spawn_templates(roots)
        if spawner == "worker":
            worker_spawns = spawn_templates([body for body in requests
                                             if not offers_card(body, root_card)])
            if f"{profile}-researcher" in root_spawns:
                raise ScenarioFailure(MODEL, f"{detail}: the root spawned the researcher itself")
            if f"{profile}-researcher" not in worker_spawns:
                raise ScenarioFailure(MODEL, f"{detail}: the worker never spawned the researcher")
        elif f"{profile}-researcher" not in root_spawns:
            raise ScenarioFailure(MODEL, f"{detail}: the root never spawned the researcher")
        summary = (f"{detail}: {spawner} spawned {profile}-researcher ({researcher}); "
                   f"{len(urls)} sources" + (f", {len(x_status_ids(urls))} X posts" if x else "")
                   + f"; {reports[0].relative_to(repo)} cites {len(set(written))} URLs")
        return summary, requests, repo

    def profile_researcher(self) -> str:
        """The route's own built-in profile: root spawns the researcher."""
        profile = {"claude": "claude-default", "codex": "codex-default",
                   "grok": "grok-default"}.get(self.route.account)
        if not profile:
            raise ScenarioFailure(HARNESS, f"no built-in profile for {self.route.account}")
        return self._research("profile_researcher", profile)[0]

    def multi_roles(self) -> str:
        """multi-default: the Grok researcher, then the Codex worker and the
        Claude reviewer on a tiny task."""
        research = self._research("multi_research", "multi-default")[0]
        out, records, segments, repo, session = self.run(
            "multi_roles",
            "Spawn the worker (template multi-default-worker, tier mutate, workspace inherit) "
            "to create hello.txt containing the single word hello. Wait for it. Then spawn "
            "the reviewer (template multi-default-reviewer, tier execute, workspace inherit) "
            "to check that hello.txt contains hello and report. Wait for it, then reply done.",
            [], profile="multi-default", timeout=1500)
        detail = f"session {session}"
        requests = capture_requests(segments)
        spawned = spawn_templates(requests)
        for role in ("worker", "reviewer"):
            if f"multi-default-{role}" not in spawned:
                raise ScenarioFailure(MODEL, f"{detail}: the root never spawned the {role}")
            model = profile_model("multi-default", role)
            if not any(request_model(body) == model
                       and not offers_card(body, "multi-default-worker")
                       for body in requests):
                raise ScenarioFailure(MODEL, f"{detail}: no {role} ({model}) request")
        if (repo / "hello.txt").read_text(errors="replace").strip().lower() != "hello":
            raise ScenarioFailure(MODEL, f"{detail}: hello.txt does not contain hello")
        return f"{research}; {detail}: codex worker wrote hello.txt, claude reviewer ran"

    def worker_spawns_researcher(self) -> str:
        """A user-authored worker whose `spawns` names the researcher, at
        max_depth = 2, spawns the researcher itself. The built-in worker is a
        leaf, so the scenario writes its own worker template."""
        profile = {"claude": "claude-default", "codex": "codex-default",
                   "grok": "grok-default"}.get(self.route.account, "multi-default")

        def depth_two(home: Path) -> None:
            config = home / "config.toml"
            text = config.read_text()
            if "max_depth = 1\n" not in text:
                raise ScenarioFailure(HARNESS, "preset config has no max_depth = 1")
            config.write_text(text.replace("max_depth = 1\n", "max_depth = 2\n", 1))
            worker = home / "agents" / f"{profile}-worker.toml"
            text = worker.read_text()
            if "spawns" in text:
                raise ScenarioFailure(HARNESS, f"{worker.name} already has spawns")
            worker.write_text(text + "spawns = " + toml_array([f"{profile}-researcher"]) + "\n")

        return self._research("worker_spawns_researcher", profile, spawner="worker",
                              config_edit=depth_two)[0]

    # --- wide trees: 24 children under one per-account request budget ------------

    def wide_fanout(self) -> str:
        """A root with max_children 24 spawns 24 reviewers in one batch, each over
        its own directory; at most 8 model requests per account are in flight."""
        width, budget = 24, 8
        dirs = wide_dirs(width)
        base = self.env.artifacts / "wide_fanout"
        repo = base / "repo"
        if not repo.exists():
            files = {"README.md": "wide_fanout scenario workspace\n"}
            for name, source in dirs:
                files[f"{name}.Cargo.toml"] = source.read_text(errors="replace")
            live.git_repo(repo, files)

        def wide(home: Path) -> None:
            config = home / "config.toml"
            text = config.read_text()
            if "max_children = 4\n" not in text:
                raise ScenarioFailure(HARNESS, "write_home config has no max_children = 4")
            config.write_text(text.replace(
                "max_children = 4\n",
                f"max_children = {width}\nmax_nodes = {width + 1}\n"
                f"max_concurrent_requests_per_account = {budget}\n", 1))

        names = ", ".join(name for name, _ in dirs)
        out, records, segments, _, session = self.run(
            "wide_fanout",
            f"Spawn {width} workers in one batch, one per manifest ({names}): call spawn_agent "
            f"{width} times in a single response, template worker, tier mutate, workspace "
            "inherit. Tell each worker: read <dir>.Cargo.toml, do not change it, and write "
            "findings/<dir>.md with one line naming the package and its dependency count; then "
            "submit_result with that line. Then call wait_agent until no_pending_children, and "
            "reply with the number of workers that submitted.",
            [], before=wide, timeout=2400)
        detail = f"session {session}"
        spawned = tool_calls(records, "spawn_agent")
        if len(spawned) < width:
            raise ScenarioFailure(MODEL, f"{detail}: {len(spawned)} spawn_agent calls, "
                                         f"expected {width}")
        missing = [name for name, _ in dirs if not (repo / "findings" / f"{name}.md").is_file()]
        if missing:
            raise ScenarioFailure(MODEL, f"{detail}: no findings file for {missing}")
        home = base / "home"
        peak, attempts = peak_in_flight(home)
        if peak > budget:
            raise ScenarioFailure(PROVIDER, f"{detail}: {peak} model requests in flight, "
                                            f"budget {budget}")
        if attempts <= width:
            raise ScenarioFailure(HARNESS, f"{detail}: only {attempts} model attempts recorded")
        return (f"{detail}: {len(spawned)} spawns, {width} findings files, {attempts} model "
                f"requests, peak {peak} in flight (budget {budget})")


def wide_dirs(width: int) -> list[tuple[str, Path]]:
    """`width` distinct crate manifests of this checkout, named by directory."""
    manifests = sorted(live.ROOT.glob("crates/**/Cargo.toml"))
    picked = []
    for manifest in manifests:
        name = manifest.parent.name
        if name not in {seen for seen, _ in picked} and "target" not in manifest.parts:
            picked.append((name, manifest))
    if len(picked) < width:
        raise ScenarioFailure(HARNESS, f"only {len(picked)} crate directories")
    return picked[:width]


def peak_in_flight(home: Path) -> tuple[int, int]:
    """(peak concurrent model attempts, attempt count) from the home's telemetry."""
    import sqlite3
    events = []
    for path in sorted(home.rglob("telemetry*.sqlite3")):
        with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as db:
            for started, finished in db.execute(
                    "SELECT started_at, finished_at FROM model_attempts "
                    "WHERE started_at IS NOT NULL AND finished_at IS NOT NULL"):
                events += [(started, 1), (finished, -1)]
    peak = current = 0
    # A finish at the same instant as a start frees the slot first.
    for _, delta in sorted(events, key=lambda event: (event[0], event[1])):
        current += delta
        peak = max(peak, current)
    return peak, len(events) // 2



PROFILE_DIRS = {"codex-default": "codex", "claude-default": "claude",
                "grok-default": "grok", "multi-default": "multi"}
PROFILE_ROLES = (("root", "root-agent"), ("worker", "worker"), ("reviewer", "reviewer"),
                 ("researcher", "researcher"))


def write_profile_home(home: Path, profile: str) -> None:
    """The home applying a built-in profile writes: `<profile>-<role>`
    templates from `bin/plak/presets/<dir>` and the preset's `config.toml`
    pointed at them."""
    source = live.ROOT / "bin/plak/presets" / PROFILE_DIRS[profile]
    (home / "agents").mkdir(parents=True, exist_ok=True)
    home.chmod(0o700)
    for role, stem in PROFILE_ROLES:
        (home / "agents" / f"{profile}-{role}.toml").write_text(
            (source / "agents" / f"{stem}.toml").read_text())
    config = (source / "config.toml").read_text()
    config = config.replace('agent = "root-agent"', f'agent = "{profile}-root"', 1)
    config = re.sub(r"^working_set = .*$", "working_set = " + toml_array(
        f"{profile}-{role}" for role, _ in PROFILE_ROLES[1:]), config, count=1, flags=re.M)
    (home / "config.toml").write_text(config)


def profile_root_flags(profile: str) -> list[str]:
    path = live.ROOT / "bin/plak/presets" / PROFILE_DIRS[profile] / "agents/root-agent.toml"
    root = tomllib.loads(path.read_text())
    flags = ["--account", root["account"], "--model", root["model"]]
    return flags + (["--effort", root["effort"]] if root.get("effort") else [])


def profile_model(profile: str, role: str) -> str:
    stem = dict(PROFILE_ROLES)[role]
    path = live.ROOT / "bin/plak/presets" / PROFILE_DIRS[profile] / "agents" / f"{stem}.toml"
    return tomllib.loads(path.read_text())["model"]


def request_model(body: object) -> str | None:
    return body.get("model") if isinstance(body, dict) else None


def _tools_text(body: object) -> str:
    """The request's tool catalog: its `tools`, plus the instructions and
    system/developer items, where a Code Mode cell declares its tools."""
    if not isinstance(body, dict):
        return ""
    parts = [body.get("tools"), body.get("instructions"), body.get("system")]
    items = body.get("input") or body.get("messages")
    if isinstance(items, list):
        parts += [item for item in items if isinstance(item, dict)
                  and item.get("role") in ("system", "developer")]
    return json.dumps([part for part in parts if part])


def offered(body: object, tool: str) -> bool:
    """Whether a captured request's tool list names `tool` (or its other
    model-visible spelling): a direct tool or one inside a Code Mode cell."""
    text = _tools_text(body)
    # A direct tool by name, or a Code Mode cell catalog entry `name(`; a
    # spawn card's `· tools <names>` capability line is neither.
    return any(f'"name": "{name}"' in text or f"{name}(" in text
               for name in RETRIEVAL_NAMES.get(tool, (tool,)))


def offers_card(body: object, template: str) -> bool:
    """Whether the request's spawn_agent menu carries `template`'s card. Only
    the root's menu carries the worker: a node's menu never holds itself."""
    return f"- {template}:" in _tools_text(body)


# A Code Mode cell's `tools.spawn_agent({template: "...", ...})`.
_CELL_SPAWN = re.compile(r"spawn_agent\(\{[^}]*?[\"']?template[\"']?\s*:\s*[\"']([a-z0-9-]+)")


def spawn_templates(bodies: list[object]) -> set[str]:
    """Templates named by spawn_agent calls the captured histories carry."""
    found: set[str] = set()

    def visit(value: object) -> None:
        if isinstance(value, dict):
            if value.get("type") == "custom_tool_call" and isinstance(value.get("input"), str):
                found.update(_CELL_SPAWN.findall(value["input"]))
            if value.get("name") == "spawn_agent":
                for key in ("input", "arguments"):
                    args = value.get(key)
                    if isinstance(args, str):
                        try:
                            args = json.loads(args)
                        except ValueError:
                            args = None
                    if isinstance(args, dict) and isinstance(args.get("template"), str):
                        found.add(args["template"])
            for item in value.values():
                visit(item)
        elif isinstance(value, list):
            for item in value:
                visit(item)

    for body in bodies:
        visit(body)
    return found


def refresh_rows(home: Path, account: str) -> list[tuple[str, int]]:
    """(phase, generation) of each credential refresh the home's telemetry holds."""
    import sqlite3
    rows = []
    for path in sorted(home.rglob("telemetry*.sqlite3")):
        with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as db:
            rows += db.execute("SELECT phase, generation FROM credential_refreshes "
                               "WHERE provider LIKE ? ORDER BY id", (f"%{account}%",)).fetchall()
    return rows


def set_tools(home: Path, tools: list[str], template: str = "root-agent") -> None:
    """List `tools` on a template `live.write_home` wrote (the root's by default)."""
    if not tools:
        return
    path = home / "agents" / f"{template}.toml"
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
