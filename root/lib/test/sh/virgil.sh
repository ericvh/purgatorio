# ip/virgild answers virgil queries on an announced UDP port in headers
# mode (ip(3)); test/netsrv virgil is the client. A malformed query gets
# no answer.
load std
d=/tmp/sh-virgil
rm -rf $d
mkdir -p $d
ip/virgild -d -a udp!127.0.0.1!0 > $d/vl >[2] $d/ve &
vp=$apid
while {! test -s $d/vl} {sleep 0}
va=udp!`{sed 's/virgild: announced //' $d/vl}
test/netsrv virgil $va 'bob?127.0.0.1'
test/netsrv virgil $va 'nobody'
kill $vp
grep -v announced $d/vl
cat $d/ve
rm -rf $d
