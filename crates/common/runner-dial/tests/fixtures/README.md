`cacophony-ik.json` is the exact matching suite entry extracted without changes
from snow v0.10.0's `tests/vectors/cacophony.txt`:
https://github.com/mcginty/snow/blob/v0.10.0/tests/vectors/cacophony.txt

Full upstream file SHA-256:
`3bde7c09a6f349ee11c825c50fcc02649f8f02a47c857a459206b357f9386cae`.
Upstream snow is MIT OR Apache-2.0; the vectors originate in cacophony's
Noise interoperability corpus. All keys and plaintexts here are public test data.
The vector's nonempty handshake payloads test the resolver directly; the dial
wrapper separately enforces its empty-payload 96/48-byte handshake.
