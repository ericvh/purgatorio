# ndb/registry and ndb/regquery.
d=/tmp/netreg
rm -rf $d
mkdir -p $d/reg
echo 'service=fs proto=styx addr=tcp!fs!styx' > $d/svc
echo 'service=printer proto=lpd' >> $d/svc
echo '	addr=tcp!gate!515 colour=no' >> $d/svc
mount {ndb/registry -f $d/svc} $d/reg
ls $d/reg
cat $d/reg/index
echo 'clock proto daytime addr tcp!gate!13 persist 1' > $d/reg/new
echo 'gone proto none' > $d/reg/new
cat $d/reg/clock
ndb/regquery -m $d/reg proto styx
ndb/regquery -m $d/reg -n addr '*'
echo 'colour no
name clock
proto
proto lpd colour yes' | ndb/regquery -m $d/reg
echo 'clock taken' > $d/reg/new
echo 'addr tcp!fs!13' > $d/reg/clock
cat $d/reg/clock
rm $d/reg/clock
rm $d/reg/fs
ls $d/reg
unmount $d/reg
rm -rf $d
