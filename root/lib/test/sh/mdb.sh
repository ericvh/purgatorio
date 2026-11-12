# mdb, and the debugger's /prog files and ctl messages (docs/debug.md,
# issue #40). /bin/test/debug tick calls debug::tick every 10 ms, which
# adds one to a word whose address it prints. The prog is stopped where
# the VM takes the stop: wasmtime at a tick, wasmi at a host call, so the
# first stop is filtered. Breakpoints stop it at the same place on both.
load std
fn pidof {ps | grep $1 | sed 's/^ *([0-9]+) .*/\1/'}
d=/tmp/sh-mdb
rm -rf $d
mkdir -p $d

echo untraced
test/debug nap &
mdb $apid < /dev/null >[2=1] | sed 's/^mdb: [0-9]+: /mdb: PID: /'
echo stop >[2=1] > /prog/$apid/ctl | sed 's/.*: //'
kill $apid
wait

echo running
trace test/debug tick >[2] $d/addr &
pid=()
while {~ $#pid 0} {
	sleep 0.1
	pid=`{pidof ' running +debug$'}
}
while {~ `{cat $d/addr} ()} {sleep 0.1}
addr=`{sed 's/counter //' $d/addr}
# The memory of a running prog cannot be touched, nor can it step.
cat /prog/$pid/heap >[2=1] | sed 's/.*: //'
echo step >[2=1] > /prog/$pid/ctl | sed 's/.*: //'
echo bpt set 0 >[2=1] > /prog/$pid/ctl | sed 's/.*: //'
echo bpt >[2=1] > /prog/$pid/ctl | sed 's/.*: //'

echo breakpoints
# Stopped at debug::tick, the word is written and read back; at the next
# stop there it is one more.
{
	echo b debug::tick
	echo b nosuch
	echo b
	echo c
	echo t
	echo w $addr 2a 0 0 0
	echo x $addr 4
	echo c
	echo x $addr 4
	echo x 0xffffffff 4
	echo s
	echo d debug::tick
	echo b
	echo zap
	echo q
} | mdb $pid >[2=1] | sed 's/^stopped in (debug::inner|__wasi_poll_oneoff)$/stopped/' | sed 's/^[0-9a-f]+ /ADDR /'
# Detached, it runs on.
sed 's/^[0-9]+ [0-9]+ [0-9]+ [^ ]+ /PID PPID PGRP USER /' /prog/$pid/status

echo kill
echo k | mdb $pid | sed 's/^stopped in (debug::inner|__wasi_poll_oneoff)$/stopped/'
wait
echo debug `{ps | grep -c ' debug$'}
mdb >[2=1]
rm -rf $d
echo done
