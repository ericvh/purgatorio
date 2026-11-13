# ip/bootpd answers BOOTP and DHCP requests from a network database on
# an announced loopback port; test/netsrv bootp is the client.
load std
d=/tmp/sh-bootpd
rm -rf $d
mkdir -p $d
echo 'sys=fs dom=fs.example.org ip=10.0.0.2 ether=00a0c9e1b2c3 bootf=/kern/fs
sys=term ip=10.0.0.7 ether=00:a0:c9:00:00:07 lease=3600
sys=gate ip=10.0.0.1
sys=signer ip=10.0.1.5
ipnet=lan ip=10.0.0.0 ipmask=255.255.255.0
	ipgw=gate fs=fs dns=10.0.0.1 dnsdomain=example.org
ipnet=site ip=10.0.0.0 ipmask=255.255.0.0 SIGNER=signer ipgw=10.0.255.254' > $d/db
ip/bootpd -d -f $d/db -a udp!127.0.0.1!0 > $d/bl >[2] $d/be &
bp=$apid
while {! test -s $d/bl} {sleep 0}
ba=udp!`{sed 's/bootpd: announced //' $d/bl}
# BOOTP gets Inferno's vendor area; DHCP gets options.
test/netsrv bootp $ba 00a0c9e1b2c3
test/netsrv bootp $ba 00a0c9000007
test/netsrv bootp $ba 00a0c9000007 discover
test/netsrv bootp $ba 00a0c9000007 request 10.0.0.7
test/netsrv bootp $ba 00a0c9000007 request 10.0.0.8
test/netsrv bootp $ba 00a0c9e1b2c3 inform 10.0.0.2
# An unknown client gets no answer.
test/netsrv bootp $ba 0000deadbeef
kill $bp
grep -v announced $d/bl
ip/bootpd -f $d/nosuch -a udp!127.0.0.1!0
echo $status
rm -rf $d
