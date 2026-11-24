# Portable session export fixtures

`session-export-v13.json` is a synthetic full-history fixture, adapted
from `session-export-v12.json` with the `portable_session` document kind.
It covers neutral history, runner placement and a saved root profile.

`session-export-v14-default.json` pins the current writer output;
`session-export-v13-default.json` pins the importable predecessor.
The retired `session-export-v12-default.json` fixture is refusal evidence.

The previous-version reader must preserve the history exactly; ordinary file
import still creates new identities and strips provider replay.
