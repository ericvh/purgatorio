# /bin/echo makes one write of the whole line, as Inferno's echo(1) does.
# A file that refuses the write, or takes only part of it, sees that one
# write and no more, and echo reports a write error.
load std file2chan
d=/tmp/echowrite
rm -rf $d
mkdir -p $d
file2chan $d/bad {} {echo write ${rget data} >> $d/writes; rerror 'no writes'}
bad=$apid
file2chan $d/short {} {echo write at ${rget offset} >> $d/writes; rwrite $tag 2}
short=$apid
file2chan $d/good {} {echo write ${rget data} >> $d/writes}
good=$apid

/bin/echo hello >[2=1] > $d/bad
echo status $status
/bin/echo -n hello >[2=1] > $d/bad
echo status $status
/bin/echo hello >[2=1] > $d/short
echo status $status
/bin/echo -n hello there > $d/good
echo status $status
/bin/echo -n > $d/good
echo status $status
cat $d/writes
kill $bad $short $good
rm -rf $d
