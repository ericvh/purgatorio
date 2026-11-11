# cpu on loopback (issue #110). auxi/cpuslave forks the session's
# namespace, binds the client's /dev from the 9P mount before /dev, runs
# a command that starts children of its own, and exits. Nothing of it may
# outlive the session: the kernel's test mode fails when a thread does.
load std
d=/tmp/sh-cpufork
rm -rf $d
mkdir -p $d
echo client > $d/f
styxlisten -A tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm >[2] $d/l &
lpid=$apid
while {! test -s $d/l} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l}
cpu -A $a 'ns | grep client/dev; cat /n/client/tmp/sh-cpufork/f; sh -c ''echo nested | cat''' </dev/null
echo status $status
cpu -A $a 'cat /n/client/tmp/sh-cpufork/nosuch' </dev/null
echo status $status
kill $lpid
rm -rf $d
