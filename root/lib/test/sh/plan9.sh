# 9export and import -9: Plan 9's exportfs opening (the tree's name, then
# OK) in front of 9P, and p9any authentication before it, both ends ours,
# on loopback.
load std
d=/tmp/sh-plan9
rm -rf $d
mkdir -p $d/tree/sub $d/n
echo hello > $d/tree/sub/f
styxlisten tcp!127.0.0.1!0 /bin/9export.wasm -A >[2] $d/l &
lpid=$apid
while {! test -s $d/l} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l}
import -A -9 $d/tree $a $d/n && cat $d/n/sub/f
echo more > $d/n/sub/g
cat $d/tree/sub/g
unmount $d/n
import -A -9 $d/tree/sub $a $d/n && ls $d/n
unmount $d/n
import -A -9 $d/nosuch $a $d/n
import -A -9 $d/tree/sub/f $a $d/n
kill $lpid
# Read-only.
styxlisten tcp!127.0.0.1!0 /bin/9export.wasm -A -r >[2] $d/l2 &
lpid=$apid
while {! test -s $d/l2} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l2}
import -A -9 $d/tree $a $d/n && cat $d/n/sub/g
echo no > $d/n/sub/h
unmount $d/n
kill $lpid
# p9any: with -k the connection is authenticated with p9sk1 before the
# tree is named. The client's tickets come from auth/authsrv, which knows
# both keys, or, holding the server's own key, it makes them itself.
echo 'key proto=p9sk1 dom=pdom user=bootes !password=srvpw' > $d/srv.keys
echo 'key proto=p9sk1 dom=pdom user=glenda !password=glendapw' > $d/cli.keys
echo 'key proto=p9sk1 dom=pdom user=glenda !password=wrong' > $d/bad.keys
echo 'key proto=p9sk1 dom=pdom user=eve !password=evepw' > $d/eve.keys
cat $d/srv.keys $d/cli.keys > $d/as.keys
styxlisten tcp!127.0.0.1!0 /bin/auth/authsrv.wasm -k $d/as.keys >[2] $d/la &
aspid=$apid
styxlisten tcp!127.0.0.1!0 /bin/9export.wasm -k $d/srv.keys >[2] $d/l3 &
lpid=$apid
while {! test -s $d/la || ! test -s $d/l3} {sleep 0}
as=tcp!`{sed 's/styxlisten: //' $d/la}
a=tcp!`{sed 's/styxlisten: //' $d/l3}
import -k $d/cli.keys -T $as -9 $d/tree $a $d/n && cat $d/n/sub/f
unmount $d/n
import -k $d/srv.keys -9 $d/tree $a $d/n && cat $d/n/sub/g
unmount $d/n
import -k $d/bad.keys -T $as -9 $d/tree $a $d/n
import -k $d/eve.keys -T $as -9 $d/tree $a $d/n
import -k $d/cli.keys -9 $d/tree $a $d/n
import -9 $d/tree $a $d/n
kill $lpid $aspid
# Neither -A nor -k, or both.
9export </dev/null
9export -A -k $d/srv.keys </dev/null
rm -rf $d
