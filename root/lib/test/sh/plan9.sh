# 9export and import -9: Plan 9's exportfs opening (the tree's name, then
# OK) in front of 9P, both ends ours, on loopback.
load std
d=/tmp/sh-plan9
rm -rf $d
mkdir -p $d/tree/sub $d/n
echo hello > $d/tree/sub/f
styxlisten tcp!127.0.0.1!0 /bin/9export.wasm -A >[2] $d/l &
lpid=$apid
while {! test -s $d/l} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l}
import -9 $d/tree $a $d/n && cat $d/n/sub/f
echo more > $d/n/sub/g
cat $d/tree/sub/g
unmount $d/n
import -9 $d/tree/sub $a $d/n && ls $d/n
unmount $d/n
import -9 $d/nosuch $a $d/n
import -9 $d/tree/sub/f $a $d/n
kill $lpid
# Read-only.
styxlisten tcp!127.0.0.1!0 /bin/9export.wasm -A -r >[2] $d/l2 &
lpid=$apid
while {! test -s $d/l2} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l2}
import -9 $d/tree $a $d/n && cat $d/n/sub/g
echo no > $d/n/sub/h
unmount $d/n
kill $lpid
# Plan 9's authentication is not there to offer.
9export </dev/null
9export -x </dev/null
rm -rf $d
