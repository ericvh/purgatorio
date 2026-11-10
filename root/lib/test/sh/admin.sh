# mntgen, ndb/query, logfile and one cron pass.
load std
d=/tmp/admin
rm -rf $d
mkdir -p $d/n

# mntgen: any name walked to is a directory, listed while it is in use.
mount {mntgen} $d/n
ls $d/n
sleep 2 < $d/n/held &
sleep 0.5
ls $d/n
wait
ls $d/n
ls -ld $d/n/anything
ls -ld $d/n/anything/below
mkdir $d/n/new
memfs $d/n/x
echo in memfs > $d/n/x/f
cat $d/n/x/f
ls $d/n
unmount $d/n/x
ls $d/n

# ndb/query against /lib/ndb/local.
ndb/query sys fs
ndb/query sys gate ip
ndb/query -a sys gate ip
ndb/query ip 10.0.1.1 sys
ndb/query sys gate ether
ndb/query ip 10.0.0.2 port
ndb/query infernosite
ndb/query ip 10.0.0.9
ndb/query -a ip
ndb/query sys nosuch
echo miss $status
ndb/query -f $d/nodb sys fs
echo $status

# logfile: a reader gets what is stored; old bytes are dropped.
logfile -16 $d/log
echo abcdef > $d/log
echo ghijklm > $d/log
cat $d/log > $d/out1 >[2] /dev/null &
r=$apid
sleep 1
kill $r
cat $d/out1
echo nopq >> $d/log
cat $d/log > $d/out2 >[2] /dev/null &
r=$apid
sleep 1
kill $r
cat $d/out2
echo 01234567890123456789 > $d/log
echo $status

# cron -t: one pass at Mon Nov 09 2026 10:15 GMT.
echo '15 10 * * * echo every day at 10:15' > $d/crontab
echo '*/5 * * * 1 echo five minutes on Monday' >> $d/crontab
echo '0 10 * * * echo not now' >> $d/crontab
echo '15 10 9 11 * echo nov 9' >> $d/crontab
echo '15 10 1 * 1 echo first or monday' >> $d/crontab
echo '15 10 1 * 2 echo first or tuesday' >> $d/crontab
echo 'bad line' >> $d/crontab
cron -f $d/crontab -t 1794219300
rm -rf $d/out1 $d/out2 $d/crontab
