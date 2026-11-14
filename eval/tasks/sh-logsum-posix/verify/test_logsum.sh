#!/bin/sh
# Hidden check for the sh-logsum-posix task: runs bin/logsum with sh (dash in the
# eval image) and compares it with what the original script printed under bash. A
# `bash` first on PATH records whether the script runs bash.

set -u
status=0
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
mkdir "$dir/stub"
cat >"$dir/stub/bash" <<STUB
#!/bin/sh
touch "$dir/bash-was-run"
exit 97
STUB
chmod +x "$dir/stub/bash"
PATH="$dir/stub:$PATH"
export PATH

fail() {
  echo "FAIL: $*"
  status=1
}

# check NAME WANT_STATUS WANT_OUTPUT [ARGS...]
check() {
  name=$1 want_status=$2 want_out=$3
  shift 3
  out=$(sh bin/logsum "$@" 2>"$dir/err")
  got=$?
  [ "$got" -eq "$want_status" ] ||
    fail "$name: exit $got, want $want_status (stderr: $(cat "$dir/err"))"
  [ "$out" = "$want_out" ] || fail "$name: printed
$out
instead of
$want_out"
}

printf 'ERROR disk full\nWARN slow\ninfo started\nINFO ready\nDEBUG x\n\n  error again\nnotice y\n' >"$dir/mixed.log"
printf 'INFO a\nInfo b\nwarn c\n' >"$dir/clean.log"
: >"$dir/empty.log"

check mixed 1 "error: 2
warn: 1
info: 2
other: 2" "$dir/mixed.log"
check clean 0 "error: 0
warn: 1
info: 2
other: 0" "$dir/clean.log"
check empty 0 "error: 0
warn: 0
info: 0
other: 0" "$dir/empty.log"
check quiet-errors 1 "2" -q "$dir/mixed.log"
check quiet-clean 0 "0" -q "$dir/clean.log"
check no-args 2 ""
check only-q 2 "" -q
check missing-file 2 "" "$dir/nope.log"
check two-files 2 "" "$dir/clean.log" "$dir/clean.log"

[ "$(head -n 1 bin/logsum)" = "#!/bin/sh" ] || fail "bin/logsum must start with #!/bin/sh"
[ ! -e "$dir/bash-was-run" ] || fail "bin/logsum runs bash"
# A call by absolute path doesn't go through PATH. Comments don't count.
if sed -E 's/(^|[[:space:]])#.*$//' bin/logsum |
  grep -Eq '(^|[^[:alnum:]_./-])/(usr/)?(local/)?bin/bash'; then
  fail "bin/logsum runs /bin/bash"
fi

[ "$status" -eq 0 ] && echo "logsum: all checks passed"
exit "$status"
