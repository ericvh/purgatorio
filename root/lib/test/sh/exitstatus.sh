# Exit statuses and messages of kill, diff, time, tsort, dd and calc, as
# Inferno's commands give them (issue #121), the full names subdirectory
# commands report themselves by, and the device type of files from a
# mounted file server (issue #165).
load std
d=/tmp/sh-exitstatus
rm -rf $d
mkdir -p $d/m

# kill goes on through its list; only nothing killed is a failure.
kill
echo kill usage $status
kill -x 1 >[2] /dev/null
echo kill bad flag $status
kill 999999 999998
echo kill none $status
cp /bin/sleep.wasm $d/exitsleep.wasm
$d/exitsleep 100 &
kill 999999 $apid
echo kill some $status
$d/exitsleep 100 &
a=$apid
$d/exitsleep 100 &
b=$apid
# Each prog takes its module's name once the module starts.
for p in $a $b {
	while {! grep exitsleep /prog/$p/status > /dev/null} {sleep 0.1}
}
kill exitsleep > /dev/null
echo kill module $status
wait
kill nosuchmodule
echo kill no module $status

# diff reports a file it cannot open and sets no status.
diff $d/missing /dev/null
echo diff missing $status
diff $d/missing >[2] /dev/null
echo diff usage $status

# time sets no status, whatever happens to the command.
time
echo time usage $status
time $d/nosuchcmd
echo time missing $status
time diff $d/missing >[2] /dev/null
echo time usage of command $status

# tsort reports a cycle but does not fail.
echo 'a b
b a' | tsort
echo tsort cycle $status

# dd fails only on its arguments and files; a read error is reported,
# the counts printed, and the status is empty.
dd -if $d/missing
echo dd open $status
dd -if /dev/null -of $d/missing/x
echo dd create $status
dd -foo 1
echo dd bad arg $status
dd -ibs 0 -if /dev/null
echo dd counts $status
dd -ibs 10 -if /
echo dd read error $status

# calc reports an error and goes on.
calc '1+'
echo calc error $status

# Subdirectory commands use their full names.
disk/chkdisk
ndb/query -f $d/missing sys

# Files from a mounted file server are of type M, numbered by the mount.
memfs $d/m
echo hi > $d/m/f
mkdir $d/m/sub
ls -l $d/m | sed 's/ M [0-9][0-9]* / M n /'
ls -ld $d/m | sed 's/ M [0-9][0-9]* / M n /'
unmount $d/m
rm -r $d
