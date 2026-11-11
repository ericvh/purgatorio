# webgrab, whois, telnet, ip/sntp and ip/tftpd against loopback servers
# (test/netsrv) that the script starts and stops itself.
load std
d=/tmp/netcmds
rm -rf $d
mkdir -p $d/tftp
cd $d

# webgrab: plain, redirected, chunked and HTML pages, the request it makes.
h=`{test/netsrv http}
u=http://127.0.0.1:^${hd $h}
webgrab $u/hello.txt
cat hello.txt
webgrab -o moved $u/moved
cat moved.txt
webgrab $u/chunked
cat chunked.txt
webgrab $u/
grep -v fetched webgrab.html
grep -c 'fetched from' webgrab.html
webgrab -o req $u/headers
grep -v Host req.txt
if {webgrab $u/nope >[2] /dev/null} {echo fetched} {echo not found}
webgrab https://example.test/
ls

# whois and telnet.
w=`{test/netsrv whois}
whois -h tcp!127.0.0.1!^${hd $w} example.test
t=`{test/netsrv telnet}
echo bob | telnet tcp!127.0.0.1!^${hd $t}

# ip/sntp asks without setting the clock.
n=`{test/netsrv ntp}
ip/sntp -d -i udp!127.0.0.1!^${hd $n}

# ip/tftpd serves $d/tftp; test/netsrv tftp is the client.
echo served by tftpd > tftp/file
chmod 644 tftp/file
echo private > tftp/secret
chmod 600 tftp/secret
touch tftp/up
chmod 666 tftp/up
# The port comes from the system, so runs in parallel do not collide;
# -d's first line says which it is, and its traces go to $d/tl too.
ip/tftpd -d -p 127.0.0.1!0 -h $d/tftp > $d/tl &
tp=$apid
while {! test -s $d/tl} {sleep 0}
ta=udp!`{sed 's/tftpd: announced //' $d/tl}
test/netsrv tftp get $ta file
test/netsrv tftp get $ta missing
test/netsrv tftp get $ta secret
test/netsrv tftp get $ta ../netcmds/tftp/file
test/netsrv tftp put $ta up uploaded by tftp
cat tftp/up
test/netsrv tftp put $ta file over

kill $tp ${tl $h} ${tl $w} ${tl $t} ${tl $n}
cd /
rm -rf $d
