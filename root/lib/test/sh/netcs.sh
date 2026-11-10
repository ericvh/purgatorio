# ndb/cs, ndb/csquery, ndb/dns and ndb/dnsquery, with a DNS server on
# loopback (test/netsrv dns) that knows a few names under example.test.
load std
d=/tmp/netcs
rm -rf $d
mkdir -p $d

# The kernel's /net/cs, under ndb/cs.
ndb/csquery tcp!10.0.0.2!6666 tcp!*!7
ndb/cs
ndb/csquery tcp!fs!styx 'net!$FILESERVER!styx' tcp!gate!80 udp!10.1.2.3!tftp
ndb/csquery tcp!*!http tcp!fs.example.org tcp!fs!9fs
ndb/csquery tcp!fs!nosuch ipx!fs!1 '!sys=gate' '!tcp=styx port'
echo $status
echo 'tcp!gate!styx' | ndb/csquery
cs tcp!fs!styx

# ndb/dns asks the loopback server; cs asks ndb/dns.
x=`{test/netsrv dns}
echo 'ipnet=test ip=127.0.0.0 ipmask=255.0.0.0' > $d/db
echo '	dns=127.0.0.1!'^${hd $x} >> $d/db
echo 'sys=here dom=here.example.test ip=10.9.9.9' >> $d/db
ndb/dns -h -f $d/db
ndb/dnsquery www.example.test here.example.test 10.1.2.3 10.9.9.9
ndb/dnsquery nosuch.example.test
echo $status
echo 'example.test mx
alias.example.test
alias.example.test cname
example.test txt
example.test ns
www.example.test mx
here.example.test all
silent.example.test' | ndb/dnsquery
ndb/csquery tcp!www.example.test!http udp!mail.example.test!53

# Stop the servers and take their files away.
kill ${tl $x} ${hd `{ps | grep ' cs$'}} ${hd `{ps | grep ' dns$'}}
unmount /net/cs
unmount '#s' /net
ls /net
cs tcp!10.0.0.2!6666
rm -rf $d
