# Previous-version session export

`session-export-v12.json` pins the version-12 neutral history vocabulary,
including named state, runner placement and a saved root profile. It is a
synthetic, sanitized fixture, not private session data. The corresponding
version-13 writer fixture adds the closed `portable_session` document kind.

`session-export-v12-default.json` separately retains the base build's byte-exact
writer output, unchanged. Tests pin those bytes and import it as the previous
version; it is not replaced by the synthetic full-history fixture above.

The previous-version reader must preserve the history exactly; ordinary file
import still creates new identities and strips provider replay.
