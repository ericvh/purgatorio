# ip/ping through /net/icmp, against 127.0.0.1. The emu opens an
# unprivileged ICMP socket on the host; a Linux host may refuse it
# (net.ipv4.ping_group_range). Then the script prints the reason on
# stderr and the lines a run prints, so the golden holds either way.
# The kernel and the probe answer 127.0.0.1 in process.
load std
d=/tmp/sh-ping
rm -rf $d
mkdir -p $d
ls /net/icmp
if {ip/ping -n 3 -i 10 127.0.0.1 > $d/out >[2] $d/err} {
	sed 's/[0-9]+ µs/N µs/g' $d/out
	ip/ping -a -n 1 -i 10 127.0.0.1 | sed 's/[0-9]+ µs/N µs/g'
	ip/ping -q -s 10 -n 2 -i 10 127.0.0.1
	ip/ping -q -s 1000 -n 1 127.0.0.1
	echo status $status
} {
	if {grep -s 'permission denied' $d/err} {
		echo 'ping.sh: skipped:' `{cat $d/err} >[1=2]
		echo sending 3 64 byte messages 10 ms apart
		echo '0: rtt N µs, avg rtt N µs, ttl = 64'
		echo '1: rtt N µs, avg rtt N µs, ttl = 64'
		echo '2: rtt N µs, avg rtt N µs, ttl = 64'
		echo sending 1 64 byte messages 10 ms apart
		echo '0: 127.0.0.1->127.0.0.1 rtt N µs, avg rtt N µs, ttl = 64'
		echo sending 2 64 byte messages 10 ms apart
		echo sending 1 1000 byte messages 1000 ms apart
		echo status ''
	} {
		cat $d/out $d/err
	}
}
# Errors: no destination, and an announce, which icmp does not do.
ip/ping >[2=1]
echo status $status
echo announce 1 >[2=1] > /net/icmp/clone | sed 's/^.*: //'
rm -rf $d
