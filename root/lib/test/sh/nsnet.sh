# ns shows a mount on a network connection as the address the
# connection reaches, read from its remote file, as Inferno's ns does;
# ns -r shows the connection's data file instead. /prog/n/ns itself is
# the raw form: no quoting, two spaces when there are no flags, and the
# attach name last (issue #118).
load std
d=/tmp/sh-nsnet
rm -rf $d
mkdir -p $d/srv $d/n $d/c
echo hi > $d/srv/f
styxlisten tcp!127.0.0.1!0 export -A $d/srv >[2] $d/el &
epid=$apid
while {! test -s $d/el} {sleep 0}
addr=`{sed 's/styxlisten: //' $d/el}
mount -A tcp!$addr $d/n
cat $d/n/f
bind -c '#U/tmp' $d/c
ns | grep $d | sed 's/![0-9]+ /!PORT /'
ns -r | grep $d/n | sed 's,/net/tcp/[0-9]+/data,/net/tcp/N/data,'
sleep 100 &
grep $d /prog/$apid/ns | sed 's,/net/tcp/[0-9]+/data,/net/tcp/N/data,'
kill $apid
unmount $d/c
unmount $d/n
kill $epid
rm -rf $d
