import asyncio as asyncio


def _plakous_host_require_random_seed():
    # micro-monty falls back to clock seeding when entropy is disabled.
    import random

    seed = random.Random.seed
    instance = random._inst
    seeded = False

    def explicit_seed(self, a=None, version=2):
        nonlocal seeded
        if a is None:
            raise RuntimeError("unseeded random is not available in cells; pass an explicit seed")
        seed(self, a, version)
        if self is instance:
            seeded = True

    def guarded(method):
        def call(*args, **kwargs):
            if not seeded:
                raise RuntimeError(
                    "unseeded random is not available in cells; call random.seed first"
                )
            return method(*args, **kwargs)

        return call

    random.Random.seed = explicit_seed
    for name in random.__all__:
        member = getattr(random, name, None)
        if getattr(member, "__self__", None) is instance:
            setattr(random, name, instance.seed if name == "seed" else guarded(member))


_plakous_host_require_random_seed()
del _plakous_host_require_random_seed
