# cprof and mprof (docs/debug.md, issue #40). The counts are exact and
# the same on both VMs: each traced function's thunk counts its calls and
# the pages memory grew while it ran. /bin/test/debug spin calls
# debug::burn 4 times from debug::inner; grow allocates and keeps a MiB
# in debug::heap, 4 times.
load std
fn shares {sed 's/^ *[0-9.]+% +//'}

echo calls
cprof -n 0 test/debug spin >[2=1] | grep '^module|debug::(burn|inner|main)$' | shares | sed 's/calls [0-9]+ functions [0-9]+ of [0-9]+/calls N functions N of N/'
# The counts of a command's children come back with its own.
cprof -n 0 sh -c 'test/debug spin; test/debug spin' >[2=1] | grep 'debug::burn$' | shares
# -u gives the functions never called.
cprof -u test/debug spin >[2=1] | grep '^debug::(burn|down|tick)$'
# A counted prog that traps is reported and reaped.
cprof test/debug trap >[2=1] | grep '^cprof' | sed 's/^cprof: [0-9]+ /cprof: PID /' | sed 's/trap: .*unreachable.*/trap: unreachable/'
cprof >[2=1]

echo pages
mprof -n 0 test/debug grow >[2=1] | grep '^module|debug::(heap|inner|main)$' | shares
# Nothing grew: the module has no lines.
mprof test/debug spin >[2=1] | grep -c debug
mprof >[2=1]

echo files
# A prog started without counting has empty files.
trace test/debug nap &
sleep 0.2
pid=`{ps | grep ' running +debug$' | sed 's/^ *([0-9]+) .*/\1/'}
cat /prog/$pid/cprof /prog/$pid/mprof
kill $pid
wait
echo broken `{ps | grep -c ' broken +debug$'}
echo done
