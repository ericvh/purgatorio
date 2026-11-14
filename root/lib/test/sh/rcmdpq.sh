# rcmd -e x25519mlkem768 and cpu -C x25519mlkem768 use and require the
# hybrid X25519 + ML-KEM-768 key exchange (docs/rcmd.md, "Encryption");
# rstyxd -C x25519mlkem768 requires it of every client. A client that
# asks for it refuses a server that does not offer it, and a server that
# requires it refuses a client that does not ask. Both ends trust the
# test keys, bound on /lib/keys.
load std
bind /lib/test/keys /lib/keys
d=/tmp/sh-rcmdpq
rm -rf $d
mkdir -p $d
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm >[2] $d/l &
lpid=$apid
while {! test -s $d/l} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l}
rcmd -e x25519mlkem768 $a 'echo hybrid; cat /n/client/tmp/sh-rcmdpq/l > /dev/null && echo client mounted' </dev/null
rcmd -S -e x25519mlkem768 $a echo server proved </dev/null
cpu -C x25519mlkem768 $a echo cpu hybrid </dev/null
rcmd -e x25519 $a echo classic </dev/null
rcmd -A -e x25519mlkem768 $a echo no </dev/null
rcmd -e x448 $a echo no </dev/null
kill $lpid
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm -C x25519mlkem768 >[2] $d/l2 &
lpid=$apid
while {! test -s $d/l2} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l2}
rcmd $a echo no </dev/null
echo status $status
rcmd -e x25519mlkem768 $a echo required and used </dev/null
kill $lpid
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm -A >[2] $d/l3 &
lpid=$apid
while {! test -s $d/l3} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l3}
rcmd -e x25519mlkem768 $a echo no </dev/null
echo status $status
kill $lpid
auxi/rstyxd -A -C x25519mlkem768 </dev/null
unmount /lib/keys
rm -rf $d
