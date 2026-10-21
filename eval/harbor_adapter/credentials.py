"""Host-side credential discovery. Never logs or returns secret bytes to tests."""

from __future__ import annotations

import base64
import json
import os
import shlex
import sys
import fcntl
import subprocess
import time
from contextlib import contextmanager
from datetime import datetime
from pathlib import Path

from harbor_adapter.tuples import (
    ACCOUNT_SUBSCRIPTION_DIR,
    PI_CODEX_PROVIDER,
    REMOTE_HOME,
    REMOTE_XDG,
    STRIP_ENV,
)


class CredentialError(ValueError):
    pass


def require_local_docker() -> None:
    docker_host = os.environ.get("DOCKER_HOST", "")
    if docker_host and not docker_host.startswith("unix://"):
        raise CredentialError(
            "eval Harbor runs require a local Unix-socket Docker context; "
            f"DOCKER_HOST={docker_host!r} is refused"
        )


def default_xdg_state_home() -> Path:
    override = os.environ.get("XDG_STATE_HOME")
    if override:
        path = Path(override)
        if not path.is_absolute():
            raise CredentialError("XDG_STATE_HOME must be absolute")
        return path
    home = os.environ.get("HOME")
    if not home:
        raise CredentialError("HOME is unset")
    return Path(home) / ".local" / "state"


def subscription_dir(account: str, *, xdg_state_home: Path | None = None) -> Path:
    key = ACCOUNT_SUBSCRIPTION_DIR.get(account)
    if key is None:
        raise CredentialError(f"account {account!r} has no OAuth subscription dir")
    root = xdg_state_home or default_xdg_state_home()
    return root / "plak" / "subscriptions" / key


def default_claude_code_store() -> Path:
    override = os.environ.get("PLAK_CLAUDE_CODE_CREDENTIALS_DIR")
    if override:
        path = Path(override)
        if not path.is_absolute():
            raise CredentialError("PLAK_CLAUDE_CODE_CREDENTIALS_DIR must be absolute")
        return path
    home = os.environ.get("HOME")
    if not home:
        raise CredentialError("HOME is unset")
    if sys.platform == "darwin":
        raise CredentialError(
            "FrozenPmux on Darwin requires PLAK_CLAUDE_CODE_CREDENTIALS_DIR "
            "pointing at a linux .credentials.json directory; ~/.claude is the "
            "macOS keychain store and is refused"
        )
    return Path(home) / ".claude"


def require_ready_claude_code_store() -> Path:
    """Linux file OAuth for FrozenPmux. Darwin keychain-only logins fail closed.

    A structurally usable OAuth pair is required. Expired access tokens may
    refresh in the native client, so expiry is not a preflight refusal.
    """
    require_local_docker()
    directory = default_claude_code_store()
    credential = directory / ".credentials.json"
    if not credential.is_file() or credential.stat().st_size == 0:
        raise CredentialError(
            f"missing {credential}; FrozenPmux needs a linux Claude Code "
            "file store (not the macOS keychain). Run `claude auth login` in "
            "a linux cell of the selected HARBOR_ARCH with Claude 2.1.280 "
            "and set "
            "PLAK_CLAUDE_CODE_CREDENTIALS_DIR to that directory"
        )
    try:
        payload = json.loads(credential.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise CredentialError(
            "Claude Code credentials file is unreadable"
        ) from exc
    _require_claude_oauth_pair(payload, label="Claude Code credentials file")
    return directory


def _require_claude_oauth_pair(payload: object, *, label: str) -> None:
    if not isinstance(payload, dict) or not payload:
        raise CredentialError(f"{label} is empty")
    oauth = payload.get("claudeAiOauth")
    if not isinstance(oauth, dict) or not all(
        isinstance(oauth.get(key), str) and bool(oauth[key].strip())
        for key in ("accessToken", "refreshToken")
    ):
        raise CredentialError(f"{label} has no usable Claude OAuth token pair")


def replace_claude_code_store(directory: Path, candidate: Path) -> None:
    """Replace the linux pin after a cell token refresh. Never logs bytes.

    Claude Code rotates the refresh token into the in-cell pin. Harbor
    uploads a copy; without this write-back the next Darwin trial NeedsLogin.
    """
    credential = directory / ".credentials.json"
    try:
        payload = json.loads(candidate.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise CredentialError(
            "refreshed Claude Code credentials file is unreadable"
        ) from exc
    _require_claude_oauth_pair(payload, label="refreshed Claude Code credentials file")
    os.replace(candidate, credential)
    os.chmod(credential, 0o600)


def refuse_stripped_env(extra_env: dict[str, str] | None) -> None:
    for key in STRIP_ENV:
        if os.environ.get(key) or (extra_env or {}).get(key):
            raise CredentialError(
                "FrozenPmux must use Claude Code subscription OAuth; "
                f"{key} in the Harbor process environment is refused"
            )


def require_ready_oauth(account: str, *, xdg_state_home: Path | None = None) -> Path:
    require_local_docker()
    directory = subscription_dir(account, xdg_state_home=xdg_state_home)
    credential = directory / "credential.json"
    if not credential.is_file():
        raise CredentialError(
            f"missing {credential}; run `plak login {account}` on the host first"
        )
    return directory


def oauth_expiry(path: Path, account: str) -> float:
    """Read expiry metadata only; errors must never include decoded credentials."""
    try:
        document = json.loads(path.read_text())
        payload = json.loads(base64.b64decode(document["payload"], validate=True))
        expiry = payload["access_expires_at" if account == "codex" else "expires_at"]
        if type(expiry) not in (int, float) or not 0 < expiry < 1e12:
            raise ValueError
        return float(expiry)
    except (OSError, ValueError, KeyError, TypeError):
        raise CredentialError("OAuth expiry metadata unavailable") from None


def native_oauth_expiry(path: Path, harness: str, provider: str | None = None) -> float:
    """Read pinned native-store expiry metadata without exposing token claims."""
    try:
        payload = json.loads(path.read_text())
        if harness == "codex":
            token = payload["tokens"]["access_token"]
            segment = token.split(".")[1]
            expiry = json.loads(base64.urlsafe_b64decode(segment + "=" * (-len(segment) % 4)))["exp"]
        elif harness == "grok-build":
            _, entry = validate_grok_auth(path.read_bytes())
            return datetime.fromisoformat(str(entry["expires_at"]).replace("Z", "+00:00")).timestamp()
        elif harness == "pmux":
            expiry = payload["claudeAiOauth"]["expiresAt"] / 1000
        elif harness in ("pi", "opencode"):
            entry = payload[provider]
            if entry.get("type") != "oauth":
                raise ValueError
            expiry = entry["expires"] / 1000
        else:
            raise ValueError
        if type(expiry) not in (int, float) or not 0 < expiry < 1e12:
            raise ValueError
        return float(expiry)
    except (OSError, ValueError, KeyError, TypeError, IndexError, AttributeError):
        raise CredentialError("native OAuth expiry metadata unavailable") from None


def require_native_oauth_lifetime(agent, account: str, agent_timeout: float,
                                  verifier_timeout: float) -> None:
    """Vendor refresh is not a substitute for a trial's initial lifetime gate."""
    harness = getattr(agent, "_native_harness", None)
    if type(agent).__name__ == "FrozenPmux":
        harness = "pmux"
    provider = None
    if harness == "codex":
        path = agent._resolve_auth_json_path()
    elif harness == "grok-build":
        path = require_ready_grok_auth()
    elif harness == "pmux":
        path = require_ready_claude_code_store() / ".credentials.json"
    elif harness == "pi":
        provider = {"codex": "openai-codex", "grok": "xai"}.get(account)
        path = require_ready_pi_auth(provider)
    elif harness == "opencode":
        provider = {"codex": "openai", "grok": "xai"}.get(account)
        path = require_ready_opencode_auth(provider)
    else:
        return  # Plak uses trial_oauth under its host refresh lock.
    if path is None or native_oauth_expiry(Path(path), harness, provider) - time.time() < (
        agent_timeout + verifier_timeout + 300
    ):
        raise CredentialError("native OAuth lifetime cannot cover task and verifier budgets plus margin; refresh the host store first")


def replace_codex_auth(path: Path, candidate: Path, snapshot: bytes) -> None:
    """Persist a same-account refresh, never overwriting a concurrent rotation."""
    try:
        old = json.loads(snapshot)
        raw = candidate.read_bytes()
        new = json.loads(raw)
        before, after = old["tokens"], new["tokens"]
        if not before.get("account_id") or after.get("account_id") != before["account_id"]:
            raise ValueError
        if not all(isinstance(after.get(k), str) and after[k] for k in ("access_token", "refresh_token")):
            raise ValueError
        if path.read_bytes() != snapshot:
            raise ValueError
    except (OSError, ValueError, TypeError, KeyError):
        raise CredentialError("Codex OAuth refresh invalid or host changed") from None
    # The downloaded tempfile is on the same filesystem as the host pin.
    candidate.chmod(0o600)
    os.replace(candidate, path)


@contextmanager
def trial_oauth(account: str, *, agent_timeout: float, verifier_timeout: float,
                xdg_state_home: Path | None = None):
    directory = require_ready_oauth(account, xdg_state_home=xdg_state_home)
    # Keep the lock through upload, so digest/lifetime and uploaded bytes cannot
    # straddle another trial's host refresh.
    fd = os.open(directory.parent / ("." + directory.name + ".harbor-refresh.lock"), os.O_CREAT | os.O_RDWR, 0o600)
    with os.fdopen(fd, "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        required = agent_timeout + verifier_timeout + 300
        if oauth_expiry(directory / "credential.json", account) - time.time() < required:
            env = dict(os.environ)
            if xdg_state_home is not None:
                env["XDG_STATE_HOME"] = str(xdg_state_home)
            try:
                refreshed = subprocess.run(["plak", "refresh", account], env=env,
                                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                           timeout=120, check=False)
            except (OSError, subprocess.TimeoutExpired):
                raise CredentialError("host OAuth refresh failed") from None
            if refreshed.returncode:
                raise CredentialError("host OAuth refresh failed")
        if oauth_expiry(directory / "credential.json", account) - time.time() < required:
            raise CredentialError("OAuth lifetime cannot cover task and verifier budgets plus margin")
        yield directory


def remote_state_acl_command(user: str | int | None, *paths: str) -> str:
    """Make copied OAuth/home state owned privately by the Harbor agent user.

    `plak` refuses a PLAK_HOME that is not uid-owned mode 0700, and native
    subscription files must be uid-owned mode 0600 in a 0700 directory.
    Harbor uploads as root, so eval must chown and tighten modes before
    `plak run`.
    """
    if not paths:
        raise CredentialError("remote state acl needs at least one path")
    quoted_paths = " ".join(shlex.quote(path) for path in paths)
    quoted_home = shlex.quote(REMOTE_HOME)
    quoted_xdg = shlex.quote(REMOTE_XDG)
    quoted_subs = shlex.quote(f"{REMOTE_XDG}/plak")
    parts: list[str] = []
    if user is not None:
        parts.append(f"chown -R {shlex.quote(str(user))} {quoted_paths}")
    else:
        parts.append(f'chown -R "$(id -u)" {quoted_home} {quoted_xdg}')
    quoted_secrets = shlex.quote(f"{REMOTE_HOME}/secrets")
    parts.append(f"chmod 0700 {quoted_home} {quoted_xdg}")
    parts.append(
        f"if [ -d {quoted_subs} ]; then "
        f"find {quoted_subs} -type d -exec chmod 0700 {{}} + && "
        f"find {quoted_subs} -type f -exec chmod 0600 {{}} +; "
        "fi"
    )
    parts.append(
        f"if [ -d {quoted_secrets} ]; then "
        f"chmod 0700 {quoted_secrets} && "
        f"find {quoted_secrets} -type f -exec chmod 0600 {{}} +; "
        "fi"
    )
    return " && ".join(parts)


def default_grok_home() -> Path:
    override = os.environ.get("GROK_HOME")
    if override:
        path = Path(override)
        if not path.is_absolute():
            raise CredentialError("GROK_HOME must be absolute")
        return path
    home = os.environ.get("HOME")
    if not home:
        raise CredentialError("HOME is unset")
    return Path(home) / ".grok"


def grok_auth_json_path(*, grok_home: Path | None = None) -> Path:
    override = os.environ.get("GROK_AUTH_JSON_PATH")
    if override:
        path = Path(override)
        if not path.is_absolute():
            raise CredentialError("GROK_AUTH_JSON_PATH must be absolute")
        return path
    return (grok_home or default_grok_home()) / "auth.json"


def require_ready_grok_auth(*, grok_home: Path | None = None) -> Path:
    require_local_docker()
    auth_json = grok_auth_json_path(grok_home=grok_home)
    if not auth_json.is_file():
        raise CredentialError(
            f"missing {auth_json}; log in with the Grok CLI so ~/.grok/auth.json exists"
        )
    if auth_json.is_symlink():
        raise CredentialError("Grok OAuth auth file must be a regular file")
    validate_grok_auth(auth_json.read_bytes())
    return auth_json


def validate_grok_auth(raw: bytes) -> tuple[str, dict[str, object]]:
    """Validate the pinned CLI's one-account SuperGrok OIDC store.

    Identity values stay in memory and must never enter diagnostics.
    """
    try:
        store = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise CredentialError("Grok OAuth auth file is unreadable") from exc
    if not isinstance(store, dict) or len(store) != 1:
        raise CredentialError("Grok OAuth auth file must contain one account")
    route, account = next(iter(store.items()))
    if not isinstance(account, dict) or account.get("auth_mode") != "oidc":
        raise CredentialError("Grok OAuth auth file has the wrong auth mode")
    required = (
        "key", "refresh_token", "expires_at", "create_time", "principal_id", "user_id",
        "oidc_issuer", "oidc_client_id",
    )
    if any(
        not isinstance(account.get(name), str) or not account[name].strip()
        for name in required
    ):
        raise CredentialError("Grok OAuth auth file has incomplete credentials")
    if route != f"https://auth.x.ai::{account['oidc_client_id']}":
        raise CredentialError("Grok OAuth auth file has the wrong route")
    if account["oidc_issuer"] != "https://auth.x.ai":
        raise CredentialError("Grok OAuth auth file has the wrong issuer")
    try:
        expiry = datetime.fromisoformat(account["expires_at"].replace("Z", "+00:00"))
        created = datetime.fromisoformat(account["create_time"].replace("Z", "+00:00"))
    except ValueError as exc:
        raise CredentialError("Grok OAuth auth file has invalid timestamps") from exc
    if expiry.tzinfo is None or created.tzinfo is None:
        raise CredentialError("Grok OAuth auth file has invalid timestamps")
    return route, account


def replace_grok_auth(path: Path, candidate: Path, original: bytes) -> bool:
    """Persist only a valid same-account in-cell refresh over an unchanged host file."""
    old_route, old = validate_grok_auth(original)
    if candidate.is_symlink() or path.is_symlink():
        raise CredentialError("Grok OAuth credential path is a symlink")
    try:
        fresh = candidate.read_bytes()
    except OSError as exc:
        raise CredentialError("Grok OAuth refresh could not be read") from exc
    new_route, new = validate_grok_auth(fresh)
    if new_route != old_route or any(
        new[name] != old[name]
        for name in ("auth_mode", "principal_id", "user_id", "oidc_issuer", "oidc_client_id")
    ):
        raise CredentialError("Grok OAuth refresh changed account identity")
    if fresh == original:
        if path.read_bytes() != original:
            raise CredentialError("Grok OAuth host credential changed during trial")
        return False
    for field in ("expires_at", "create_time"):
        previous = datetime.fromisoformat(old[field].replace("Z", "+00:00"))
        updated = datetime.fromisoformat(new[field].replace("Z", "+00:00"))
        if updated < previous:
            raise CredentialError("Grok OAuth refresh regressed credential time")
    if path.read_bytes() != original:
        raise CredentialError("Grok OAuth host credential changed during trial")
    os.chmod(candidate, 0o600)
    os.replace(candidate, path)
    return True


def remote_uploaded_auth_acl_command(
    user: str | int | None,
    remote_auth_path: str,
) -> str:
    """Tighten ownership and mode on an uploaded vendor auth file.

    Agent-home directories are chmod'd in the agent shell so `$HOME` is not
    expanded as root.
    """
    quoted_auth = shlex.quote(remote_auth_path)
    parts: list[str] = []
    if user is not None:
        parts.append(f"chown {shlex.quote(str(user))} {quoted_auth}")
    parts.append(f"chmod 0600 {quoted_auth}")
    return " && ".join(parts)


def remote_claude_securestorage_acl_command(
    user: str | int | None,
    directory: str,
) -> str:
    """Owner-only Claude Code pin dir so pmuxd can refresh the file store.

    Harbor mkdir/upload is root. A 0600 file in a root 0755 directory is
    readable once; token refresh writes a temp file then renames.
    """
    quoted_dir = shlex.quote(directory)
    quoted_cred = shlex.quote(f"{directory.rstrip('/')}/.credentials.json")
    parts: list[str] = []
    if user is not None:
        parts.append(f"chown -R {shlex.quote(str(user))} {quoted_dir}")
    parts.append(f"chmod 0700 {quoted_dir}")
    parts.append(f"chmod 0600 {quoted_cred}")
    return " && ".join(parts)


def remote_grok_auth_acl_command(
    user: str | int | None,
    remote_auth_path: str,
) -> str:
    if user is None:
        raise CredentialError("native Grok needs an agent user for private auth state")
    directory = str(Path(remote_auth_path).parent)
    return (
        f"chown {shlex.quote(str(user))} {shlex.quote(directory)} "
        f"{shlex.quote(remote_auth_path)} && "
        f"chmod 0700 {shlex.quote(directory)} && "
        f"chmod 0600 {shlex.quote(remote_auth_path)}"
    )


def _auth_json_providers(path: Path) -> set[str]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise CredentialError(f"unreadable auth file at {path}") from exc
    if not isinstance(payload, dict):
        raise CredentialError(f"{path} is not a JSON object")
    return {str(key) for key in payload}


def _require_auth_provider(path: Path, provider: str, *, how: str) -> Path:
    providers = _auth_json_providers(path)
    if provider not in providers:
        raise CredentialError(f"{how} has no {provider} login")
    return path


def default_pi_agent_dir() -> Path:
    override = os.environ.get("PI_CODING_AGENT_DIR")
    if override:
        path = Path(override)
        if not path.is_absolute():
            raise CredentialError("PI_CODING_AGENT_DIR must be absolute")
        return path
    home = os.environ.get("HOME")
    if not home:
        raise CredentialError("HOME is unset")
    return Path(home) / ".pi" / "agent"


def pi_auth_json_path(*, pi_agent_dir: Path | None = None) -> Path:
    override = os.environ.get("PI_AUTH_JSON_PATH")
    if override:
        path = Path(override)
        if not path.is_absolute():
            raise CredentialError("PI_AUTH_JSON_PATH must be absolute")
        return path
    return (pi_agent_dir or default_pi_agent_dir()) / "auth.json"


def require_ready_pi_auth(
    provider: str, *, pi_agent_dir: Path | None = None
) -> Path:
    require_local_docker()
    auth_json = pi_auth_json_path(pi_agent_dir=pi_agent_dir)
    if not auth_json.is_file():
        raise CredentialError(
            f"missing {auth_json}; log in with the Pi CLI so ~/.pi/agent/auth.json exists"
        )
    _require_auth_provider(auth_json, provider, how="Pi auth.json")
    if provider == PI_CODEX_PROVIDER:
        try:
            entry = json.loads(auth_json.read_text(encoding="utf-8"))[provider]
        except (OSError, UnicodeError, json.JSONDecodeError, KeyError) as exc:
            raise CredentialError("Pi Codex OAuth credential is unreadable") from exc
        if not isinstance(entry, dict) or entry.get("type") != "oauth":
            raise CredentialError("Pi openai-codex login is not OAuth")
        if any(
            not isinstance(entry.get(field), str) or not entry[field].strip()
            for field in ("access", "refresh")
        ):
            raise CredentialError("Pi openai-codex OAuth login is incomplete")
    return auth_json


def _pi_codex_entry(raw: bytes) -> tuple[dict[str, object], dict[str, object], str]:
    try:
        store = json.loads(raw)
        entry = store[PI_CODEX_PROVIDER]
    except (UnicodeDecodeError, json.JSONDecodeError, KeyError, TypeError) as exc:
        raise CredentialError("Pi Codex OAuth credential is unreadable") from exc
    if not isinstance(store, dict) or not isinstance(entry, dict) or entry.get("type") != "oauth":
        raise CredentialError("Pi openai-codex login is not OAuth")
    if any(
        not isinstance(entry.get(field), str) or not entry[field].strip()
        for field in ("access", "refresh")
    ):
        raise CredentialError("Pi openai-codex OAuth login is incomplete")
    expires = entry.get("expires")
    if not isinstance(expires, (int, float)) or isinstance(expires, bool):
        raise CredentialError("Pi openai-codex OAuth expiry is invalid")
    try:
        segment = entry["access"].split(".")[1]
        segment += "=" * (-len(segment) % 4)
        claims = json.loads(base64.urlsafe_b64decode(segment))
        account_id = claims["https://api.openai.com/auth"]["chatgpt_account_id"]
    except (IndexError, KeyError, TypeError, ValueError, json.JSONDecodeError) as exc:
        raise CredentialError("Pi openai-codex account identity is unreadable") from exc
    if not isinstance(account_id, str) or not account_id:
        raise CredentialError("Pi openai-codex account identity is unreadable")
    if entry.get("accountId") not in (None, account_id):
        raise CredentialError("Pi openai-codex account identity is inconsistent")
    return store, entry, account_id


def replace_pi_codex_auth(path: Path, candidate: Path, original: bytes) -> bool:
    """Persist a same-account Pi Codex refresh over an unchanged host file."""
    old_store, old, old_account = _pi_codex_entry(original)
    if path.is_symlink() or candidate.is_symlink():
        raise CredentialError("Pi OAuth credential path is a symlink")
    try:
        fresh = candidate.read_bytes()
    except OSError as exc:
        raise CredentialError("Pi OAuth refresh could not be read") from exc
    new_store, new, new_account = _pi_codex_entry(fresh)
    if new_account != old_account:
        raise CredentialError("Pi OAuth refresh changed account identity")
    if {key: value for key, value in new_store.items() if key != PI_CODEX_PROVIDER} != {
        key: value for key, value in old_store.items() if key != PI_CODEX_PROVIDER
    }:
        raise CredentialError("Pi OAuth refresh changed another provider")
    if new["expires"] < old["expires"]:
        raise CredentialError("Pi OAuth refresh regressed credential expiry")
    if path.read_bytes() != original:
        raise CredentialError("Pi OAuth host credential changed during trial")
    if fresh == original:
        return False
    os.chmod(candidate, 0o600)
    os.replace(candidate, path)
    return True


def default_plak_home() -> Path:
    override = os.environ.get("PLAK_HOME")
    if override:
        path = Path(override)
        if not path.is_absolute():
            raise CredentialError("PLAK_HOME must be absolute")
        return path
    home = os.environ.get("HOME")
    if not home:
        raise CredentialError("HOME is unset")
    return Path(home) / ".plak"


def openrouter_key_path(*, plak_home: Path | None = None) -> Path:
    override = os.environ.get("PLAK_OPENROUTER_KEY_PATH")
    if override:
        path = Path(override)
        if not path.is_absolute():
            raise CredentialError("PLAK_OPENROUTER_KEY_PATH must be absolute")
        return path
    return (plak_home or default_plak_home()) / "secrets" / "openrouter.key"


def require_ready_openrouter_key(*, plak_home: Path | None = None) -> Path:
    """Host OpenRouter API-key file. Never reads or returns secret bytes."""
    require_local_docker()
    key = openrouter_key_path(plak_home=plak_home)
    if not key.is_file():
        raise CredentialError(
            f"missing {key}; write the OpenRouter API key to "
            f"{(plak_home or default_plak_home()) / 'secrets' / 'openrouter.key'}"
        )
    if key.stat().st_size == 0:
        raise CredentialError(f"empty OpenRouter key file at {key}")
    return key


def pi_cli_provider(harbor_model: str) -> str:
    """Pi's ChatGPT OAuth provider is `openai-codex`, not Harbor `openai`."""
    provider = harbor_model.split("/", 1)[0]
    mapping = {"openai": "openai-codex", "xai": "xai", "openrouter": "openrouter"}
    if provider not in mapping:
        raise CredentialError(f"Pi native-home has no provider mapping for {provider}")
    return mapping[provider]


def require_watched_openrouter_key(arm: str) -> Path:
    """Bind a paid arm to the watched key without returning secret bytes."""
    watched = require_ready_openrouter_key()
    if arm == "plak-openrouter":
        return watched
    if arm == "pi-openrouter":
        auth, kind = require_ready_pi_auth("openrouter"), "api_key"
    elif arm == "opencode-openrouter":
        auth, kind = require_ready_opencode_auth("openrouter"), "api"
    else:
        raise CredentialError("unknown watched OpenRouter arm")
    try:
        entry = json.loads(auth.read_text(encoding="utf-8"))["openrouter"]
        expected = watched.read_text(encoding="utf-8").strip()
        matches = (isinstance(entry, dict) and entry.get("type") == kind
                   and isinstance(entry.get("key"), str) and bool(expected)
                   and entry["key"] == expected)
    except (OSError, ValueError, KeyError):
        raise CredentialError("cannot verify watched OpenRouter credential") from None
    if not matches:
        raise CredentialError("OpenRouter arm credential does not match the watched key")
    return watched


def pi_cli_model_name(harbor_model: str) -> str:
    if "/" not in harbor_model:
        raise CredentialError("Harbor model must be provider/model")
    _, model_id = harbor_model.split("/", 1)
    return f"{pi_cli_provider(harbor_model)}/{model_id}"


def default_opencode_data_home() -> Path:
    override = os.environ.get("XDG_DATA_HOME")
    if override:
        path = Path(override)
        if not path.is_absolute():
            raise CredentialError("XDG_DATA_HOME must be absolute")
        return path
    home = os.environ.get("HOME")
    if not home:
        raise CredentialError("HOME is unset")
    return Path(home) / ".local" / "share"


def opencode_auth_json_path(*, data_home: Path | None = None) -> Path:
    override = os.environ.get("OPENCODE_AUTH_JSON_PATH")
    if override:
        path = Path(override)
        if not path.is_absolute():
            raise CredentialError("OPENCODE_AUTH_JSON_PATH must be absolute")
        return path
    return (data_home or default_opencode_data_home()) / "opencode" / "auth.json"


def require_ready_opencode_auth(
    provider: str, *, data_home: Path | None = None
) -> Path:
    require_local_docker()
    auth_json = opencode_auth_json_path(data_home=data_home)
    if not auth_json.is_file():
        raise CredentialError(
            f"missing {auth_json}; log in with OpenCode so "
            "~/.local/share/opencode/auth.json exists"
        )
    return _require_auth_provider(auth_json, provider, how="OpenCode auth.json")


def opencode_auth_provider(harbor_model: str) -> str:
    provider = harbor_model.split("/", 1)[0]
    if provider not in {"openai", "xai", "openrouter"}:
        raise CredentialError(
            f"OpenCode native-home has no provider mapping for {provider}"
        )
    return provider
