"""Retry Harbor docker compose once on a live Docker Hub handshake timeout.

Rung A pre-pulls the eight `alexgshaw/*` task images. Compose `up` can
still hit Hub (`context deadline exceeded`) when the daemon re-resolves
the tag. Retry `start()` once after `down`; do not retry model errors.
"""

from __future__ import annotations

from typing import override

from harbor.environments.docker.docker import DockerEnvironment

from harbor_adapter.overload import should_retry_compose


class RetryingDocker(DockerEnvironment):
    """`DockerEnvironment` that retries compose start once on Hub timeouts."""

    @override
    async def start(self, force_build: bool) -> None:
        try:
            await super().start(force_build)
        except RuntimeError as exc:
            if not should_retry_compose(str(exc)):
                raise
            try:
                await self._run_docker_compose_command(
                    ["down", "--remove-orphans"],
                    check=False,
                )
            except Exception:
                pass
            await super().start(force_build)
