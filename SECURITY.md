# Security policy

## Supported versions

Security fixes go to the latest `0.0.x` release and to `main`. Older releases
are not patched; update to the latest release.

## Reporting a vulnerability

Report privately through GitHub private vulnerability reporting:
<https://github.com/N0xMare/plak/security/advisories/new>.

Do not open a public issue, pull request or discussion about a vulnerability
before a fix is released.

Include:

- the affected version (`plak version`) or commit, and the platform;
- the account route and configuration involved, if any;
- the impact, and minimal steps to reproduce it.

Never include live credentials, authorization headers, session databases, Plak
home directories or unredacted provider transcripts. If a credential may have
been exposed, revoke or rotate it with its issuer immediately; do not wait for a
fix.

## Scope

In scope: breaks of the boundaries Plak claims in
[spec 04](docs/spec/04-security-and-boundaries.md), for example credential
leaks across accounts, routes or into logs and transcripts, file-tool
confinement escapes, auth that falls back across routes, canonical-state
corruption, and the release installer or artifacts.

Out of scope, because they are documented limits
([known limits](docs/user-guide/known-limits-and-security-model.md)): what a
model-launched process tool does with your host authority (there is no OS
sandbox and no approval prompt), credentials readable by processes running as
you (plaintext files, no OS keychain), and behaviour spec 04 states as
unenforced or convention.
