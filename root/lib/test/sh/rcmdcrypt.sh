# rcmd encrypts the connection after the proof (docs/rcmd.md). A client
# that did not ask for the clear with -A refuses a server that offers no
# session key, and -e none goes only with -A. Both ends trust the test
# keys, bound on /lib/keys.
load std
bind /lib/test/keys /lib/keys
d=/tmp/sh-rcmdcrypt
rm -rf $d
mkdir -p $d
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm -A >[2] $d/l &
lpid=$apid
while {! test -s $d/l} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l}
rcmd $a echo no </dev/null
echo status $status
rcmd -A $a echo clear </dev/null
rcmd -A -e none $a echo clear again </dev/null
kill $lpid
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm >[2] $d/l2 &
lpid=$apid
while {! test -s $d/l2} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l2}
rcmd -e chacha20poly1305 $a 'echo sealed; cat /n/client/tmp/sh-rcmdcrypt/l2 > /dev/null && echo client mounted' </dev/null
rcmd -e none $a echo no </dev/null
cpu -C clear $a echo no </dev/null
cpu -C chacha20poly1305 $a 'echo sealed too' </dev/null
kill $lpid
unmount /lib/keys
rm -rf $d
