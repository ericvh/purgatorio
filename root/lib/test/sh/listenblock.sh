# styxlisten takes a {command} block, as Inferno's listen(1) does: sh
# runs it with the connection as its standard input and output. (listen
# does too; the acceptance tests check it, on a port they choose.)
load std
d=/tmp/sh-listenblock
rm -rf $d
mkdir -p $d/srv $d/n
echo served by a block > $d/srv/f
styxlisten -A tcp!127.0.0.1!0 {export -A $d/srv} >[2] $d/el &
epid=$apid
while {! test -s $d/el} {sleep 0}
addr=`{sed 's/styxlisten: //' $d/el}
mount -A tcp!$addr $d/n
cat $d/n/f
unmount $d/n
kill $epid
rm -rf $d
