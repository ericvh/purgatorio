# stack, trace, broke and prof (docs/debug.md, issue #40).
# /bin/test/debug traps, sleeps or spins three calls down, in functions
# that are never inlined: main, outer, middle, inner (and burn, for spin).
# The stacks are the same on both VMs, because both run the same traced
# module. What differs is filtered here: the wording of a trap, pids,
# function indices and sample counts.
load std
fn pidof {ps | grep $1 | sed 's/^ *([0-9]+) .*/\1/'}

echo untraced
# A prog started without tracing has no stack, and a trap leaves nothing.
test/debug nap &
cat /prog/$apid/stack >[2=1] | sed 's/.*: //'
kill $apid
wait
test/debug trap >[2] /dev/null
echo $status | sed 's/trap: .*unreachable.*/trap: unreachable/'
echo broken `{ps | grep -c ' broken +debug$'}

echo trapped
trace test/debug trap >[2=1] | sed 's/^trace: [0-9]+ /trace: PID /' | sed 's/trap: .*unreachable.*/trap: unreachable/'
pid=`{pidof ' broken +debug$'}
echo broken $#pid
# Innermost first. The frames above main are the runtime's.
stack $pid | grep '^(exception|debug::)' | sed 's/trap: .*unreachable.*/trap: unreachable/'
stack -v $pid | grep '^debug::inner ' | sed 's/ [0-9]+ / N /'
stack $pid | grep -c '^_start$'
sed 's/trap: .*unreachable.*/trap: unreachable/' /prog/$pid/exception
# A broken prog keeps its stack, not its files.
cat /prog/$pid/fd
sed 's/^[0-9]+ [0-9]+ [0-9]+ [^ ]+ /PID PPID PGRP USER /' /prog/$pid/status
broke | sed 's/[0-9]+/PID/'
echo broken `{ps | grep -c ' broken +debug$'}
broke

echo running
# The stack of a prog blocked in the system is the one it called with.
trace test/debug nap &
pid=()
while {~ $#pid 0} {
	sleep 0.1
	pid=`{pidof ' running +debug$'}
}
while {! stack $pid | grep '^debug::inner$' > /dev/null} {sleep 0.1}
stack $pid | grep '^debug::'
cat /prog/$pid/exception
kill $pid
wait
echo broken `{ps | grep -c ' broken +debug$'}

echo real commands run traced
trace sh -c 'ls /bin/echo.wasm; echo a b c | wc -w | sed ''s/ *//''; load add; echo ${add 20 22}'
trace auth/modpolicy warn echo traced under a module policy >[2] /dev/null

echo profile
# burn is where spin spends its time, whichever way the VM samples.
prof -n 1 test/debug spin >[2=1] | sed 's/ +[0-9]+$/ N/' | sed 's/^ *[0-9.]+% +[0-9]+ //'
# The samples of a command's children come back with its own.
prof -n 1 sh -c 'test/debug spin; test/debug spin' >[2=1] | grep 'debug(\.wasm|::)' | sed 's/ +[0-9]+$/ N/' | sed 's/^ *[0-9.]+% +[0-9]+ //'
# A profiled prog that traps is reported and reaped.
prof test/debug trap >[2=1] | grep '^prof' | sed 's/^prof: [0-9]+ /prof: PID /' | sed 's/trap: .*unreachable.*/trap: unreachable/'
echo broken `{ps | grep -c ' broken +debug$'}
prof >[2=1]
echo done
