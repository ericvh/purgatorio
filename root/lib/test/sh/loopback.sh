# The loopback interface, as ip(3)'s `bind loopback /dev/null` and
# `add 127.0.0.1 255.0.0.0` make it: /net/ipifc lists it and
# /net/iproute has its routes; TCP and UDP over 127.0.0.1 reach servers
# announced on 127.0.0.1 and on every address (*). The kernel and the
# probe carry it in process, with or without a network card; the emu
# uses the host's loopback. Its interface number follows a card's, so it
# is found by its device, and left out of the route lines.
load std
lo=()
for i in /net/ipifc/* {
	if {grep -s '^loopback ' $i/status} {lo=$i}
}
cat $lo/status $lo/local
grep '^127\.' /net/iproute | sed 's/ +[0-9]+$//'
for p in tcp udp {
	for h in 127.0.0.1 '*' {
		s=`{test/netsrv echo $p $h}
		test/netsrv ask $p!127.0.0.1!^${hd $s} 'hello '^$p^' '^$h
		kill ${tl $s}
	}
}
