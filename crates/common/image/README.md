# plakous-image

Image attachment rules without a runtime. It recognizes png, jpeg, gif and webp
from their leading bytes, holds a message to its image count and byte budget
and, with the default `normalize` feature, reads a caller-named image file and
brings an image within the attachment bounds (long edge 2048 px, 3.75 MiB) or
the tool-result budget. This published-internal crate reads only the file its
caller names; presenting images stays with its callers.
