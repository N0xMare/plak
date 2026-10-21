"""Bind task images to campaign IDs; retry only transient compose setup faults."""

from __future__ import annotations

from typing import override

from harbor.environments.docker.docker import DockerEnvironment

from harbor_adapter.overload import should_retry_compose
from harbor_adapter.campaign import load_campaign
from harbor_adapter.preflight import PreflightError


class RetryingDocker(DockerEnvironment):
    """`DockerEnvironment` that retries compose start once on Hub timeouts."""

    @override
    async def start(self, force_build: bool) -> None:
        plan = load_campaign()
        if force_build:
            raise PreflightError("per-trial force-build is forbidden; freeze campaign images first")
        matches = [value for name, value in plan["tasks"].items()
                   if name.rsplit("/", 1)[-1] == self.environment_dir.parent.name]
        if len(matches) != 1:
            raise PreflightError("task has no unique campaign image binding")
        image = matches[0]["image"]
        self.task_env_config.docker_image = image
        self._env_vars.prebuilt_image_name = image
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
