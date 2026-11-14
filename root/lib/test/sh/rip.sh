# ip/rip learns routes from RIP responses that test/netsrv rip sends to
# its announced loopback port. -n says what it would change; without it,
# the routes go into /net/iproute. The lines about the host's own
# networks (Existing) vary and are left out.
load std
d=/tmp/sh-rip
rm -rf $d
mkdir -p $d
fn waitfor {
	while {! grep -s $1 $d/rl} {sleep 0}
}
ip/rip -n -d -a udp!127.0.0.1!0 > $d/rl &
rp=$apid
waitfor announced
ra=udp!`{sed -n 's/rip: announced //p' $d/rl}
# A version 1 route through the sender, then a better one through
# another gateway (version 2, with its mask and next hop).
test/netsrv rip $ra 1 203.0.113.0/0.0.0.0/0.0.0.0/3
waitfor 'add 203'
test/netsrv rip $ra 2 203.0.113.0/255.255.255.0/127.0.0.9/1 198.51.100.0/255.255.255.128/0.0.0.0/2
waitfor 'add 198'
# Withdrawn by its gateway; bad entries, and one 16 (infinity) away
# once counted, are ignored.
test/netsrv rip $ra 2 198.51.100.0/255.255.255.128/0.0.0.0/16 127.0.0.0/255.0.0.0/0.0.0.0/1 224.0.0.0/240.0.0.0/0.0.0.0/1
waitfor 'delete 198'
test/netsrv rip $ra 1 192.0.2.0/255.255.255.0/0.0.0.0/1 192.0.2.0/0.0.0.0/0.0.0.0/20
test/netsrv rip $ra 1 192.0.2.0/0.0.0.0/0.0.0.0/15
test/netsrv rip $ra 1 192.0.2.0/0.0.0.0/0.0.0.0/14
waitfor 'add 192'
kill $rp
grep -v 'Existing|announced' $d/rl
# Without -n the change is written to iproute, which lists it until the
# gateway says the network is unreachable (metric 16) and rip removes it.
fn inroute {
	grep -s '^203\.0\.113\.' /net/iproute
}
ip/rip -d -a udp!127.0.0.1!0 > $d/rl &
rp=$apid
waitfor announced
ra=udp!`{sed -n 's/rip: announced //p' $d/rl}
test/netsrv rip $ra 1 203.0.113.0/0.0.0.0/0.0.0.0/3
while {! inroute} {sleep 0}
grep '^203\.0\.113\.' /net/iproute
test/netsrv rip $ra 1 203.0.113.0/0.0.0.0/0.0.0.0/16
while {inroute} {sleep 0}
echo withdrawn
kill $rp
grep -v 'Existing|announced|started' $d/rl
ip/rip 1.2.3
rm -rf $d
