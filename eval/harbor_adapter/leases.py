"""One host-local campaign owner per subscription account, across checkouts."""
from contextlib import contextmanager, ExitStack
import fcntl
import os
from pathlib import Path
import tempfile

from harbor_adapter.preflight import PreflightError


@contextmanager
def account_leases(accounts, *, directory=None):
    directory = Path(directory or Path(tempfile.gettempdir()) / f"harbor-accounts-{os.getuid()}")
    directory.mkdir(mode=0o700, exist_ok=True)
    stat = directory.lstat()
    if directory.is_symlink() or stat.st_uid != os.getuid() or stat.st_mode & 0o077:
        raise PreflightError("unsafe account lease directory")
    with ExitStack() as stack:
        for account in sorted(set(accounts)):
            if account not in ("codex", "grok", "claude"):
                raise PreflightError("only subscription accounts may be leased")
            fd = os.open(directory / (account + ".lock"), os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
            handle = stack.enter_context(os.fdopen(fd, "w"))
            try:
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise PreflightError("subscription account is owned by another campaign") from None
        yield
