#!/bin/sh
REAL_GIT={real_git}
case "$1" in
  --version|version|help)
    [ -n "$REAL_GIT" ] || exit 127
    exec "$REAL_GIT" "$@" ;;
esac
echo "plak: git refused: this copy is a jj workspace with no git directory; git would act on the launch checkout. Use jj (jj st, jj diff, jj log, jj commit). The candidate is sealed from this copy's files." >&2
exit 128
