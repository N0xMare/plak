#!/bin/sh
# Plak occupancy jj-op fence. Basename jj only.
ALLOWED={allowed}
REAL_JJ={real_jj}
refuse() {{
  echo "plak: jj-op refused: $* affects the whole repository, not this copy; report to the parent instead" >&2
  exit 1
}}
check_config() {{
  case "$1" in
    *aliases*|*immutable_heads*|*snapshot*|*\\*) refuse "protected configuration" ;;
  esac
}}
check_track() {{
  # Fileset expressions and directories could include secret files indirectly.
  case "$1" in ''|-*|*[!A-Za-z0-9._/@+,-]*) refuse "file track requires a simple explicit non-secret path" ;; esac
  name=`printf '%s' "${{1##*/}}" | tr '[:upper:]' '[:lower:]'`
  case "$name" in
    .env.example) ;;
    .env|.env.*|.netrc|.npmrc|credentials.json|id_rsa|id_ed25519|*.pem|*.key|*.p12|*.pfx) refuse "secret file track" ;;
  esac
  [ -f "$1" ] || refuse "file track requires explicit non-secret files"
  track_count=$((track_count + 1))
}}
sub1=""
sub2=""
prev=""
at_op=0
track_count=0
global_read=0
arg_index=0
command_index=0
paths_only=0
for arg in "$@"; do
  arg_index=$((arg_index + 1))
  if [ "$paths_only" -eq 1 ] && [ "$sub1" = file ] && [ "$sub2" = track ]; then
    check_track "$arg"
    continue
  fi
  if [ -n "$prev" ]; then
    case "$prev" in
      --config) check_config "$arg" ;;
    esac
    prev=""
    continue
  fi
  # Only -R is a value-taking short global; refuse all clusters/glued values.
  case "$arg" in --*) ;; -??*) refuse "clustered or glued short options; pass values separately" ;; esac
  case "$arg" in
    -R|--repository|-R?*|--repository=*) refuse "repository escapes occupancy copy" ;;
    --ignore-immutable|--ignore-immutable=*) refuse "--ignore-immutable" ;;
    --config-file|--config-file=*) refuse "--config-file" ;;
    --config) prev=$arg ;;
    --config=*) check_config "${{arg#*=}}" ;;
    --at-op|--at-operation) at_op=1; prev=$arg ;;
    --at-op=*|--at-operation=*) at_op=1 ;;
    --color) prev=$arg ;;
    --color=*) ;;
    --help|-h|--version|-V) global_read=1 ;;
    --) paths_only=1 ;;
    -*) if [ "$sub1" = file ] && [ "$sub2" = track ]; then check_track "$arg"; fi ;;
    *)
      if [ -z "$sub1" ]; then sub1=$arg; command_index=$arg_index
      elif [ -z "$sub2" ]; then sub2=$arg
      elif [ "$sub1" = file ] && [ "$sub2" = track ]; then check_track "$arg"
      fi
      ;;
  esac
done
case "$sub1" in
  init) refuse "this occupancy copy is already a workspace of the launch repo" ;;
  undo|redo|gerrit) refuse "$sub1" ;;
  operation|op) case "$sub2" in log|show|diff) ;; *) refuse "$sub1 $sub2" ;; esac ;;
  util) case "$sub2" in completion|config-schema|markdown-help|install-man-pages|exec|snapshot) ;; *) refuse "util $sub2" ;; esac ;;
  git) case "$sub2" in root) ;; *) refuse "this occupancy copy is already a workspace of the launch repo; jj git $sub2" ;; esac ;;
  bookmark|b) case "$sub2" in list|l) ;; *) refuse "bookmark mutation" ;; esac ;;
  tag) case "$sub2" in list|l) ;; *) refuse "tag mutation" ;; esac ;;
  workspace) case "$sub2" in list|root) ;; *) refuse "workspace mutation escapes host mint" ;; esac ;;
  config) case "$sub2" in get|g|list|l|path|p) ;; *) refuse "config mutation" ;; esac ;;
  file) if [ "$sub2" = track ] && [ "$track_count" -eq 0 ]; then refuse "file track requires explicit non-secret files"; fi ;;
  abandon|absorb|arrange|bisect|commit|ci|converge|describe|desc|diff|diffedit|duplicate|edit|evolog|fix|help|interdiff|log|metaedit|new|next|parallelize|prev|rebase|resolve|restore|revert|root|run|show|sign|simplify-parents|sparse|split|squash|status|st|unsign|version) ;;
  "") [ "$global_read" -eq 1 ] || refuse "missing command or configured default alias" ;;
  *) refuse "unknown command or configured alias" ;;
esac
if [ "$at_op" -eq 1 ]; then
  case "$sub1" in
    log|show|diff|status|st|evolog|interdiff) ;;
    file) case "$sub2" in list|show|annotate|search) ;; *) refuse "--at-op file mutation" ;; esac ;;
    operation|op) case "$sub2" in log|show|diff) ;; *) refuse "--at-op operation mutation" ;; esac ;;
    *) refuse "--at-op with mutating command" ;;
  esac
fi

cwd=`pwd -P 2>/dev/null` || refuse "cannot resolve working directory"
case "$cwd" in
  "$ALLOWED"|"$ALLOWED"/*) ;;
  *) refuse "working directory escapes occupancy copy" ;;
esac
[ -d "$ALLOWED/.jj" ] && [ ! -L "$ALLOWED/.jj" ] || refuse "copy .jj is missing or linked"
probe=$cwd
while [ "$probe" != "$ALLOWED" ]; do
  if [ -e "$probe/.jj" ] || [ -L "$probe/.jj" ]; then
    refuse "nested or linked .jj escapes occupancy copy"
  fi
  probe=${{probe%/*}}
done
# Expand admitted default aliases ourselves: jj ignores CLI alias overrides.
expanded=""
case "$sub1" in b) expanded=bookmark ;; ci) expanded=commit ;; desc) expanded=describe ;; op) expanded=operation ;; st) expanded=status ;; esac
if [ -n "$expanded" ]; then
  count=$#
  index=0
  while [ "$index" -lt "$count" ]; do
    arg=$1
    shift
    index=$((index + 1))
    if [ "$index" -eq "$command_index" ]; then set -- "$@" "$expanded"
    else set -- "$@" "$arg"
    fi
  done
fi
exec "$REAL_JJ" --config {base_config} --config {immutable_config} --config {snapshot_config} "$@"
