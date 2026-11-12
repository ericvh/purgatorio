# rawdbfs: records of any bytes as numbered files, kept as a log that
# grows by appends; -x's index and stats, -l and -u.
load std
d=/tmp/sh-rawdbfs
rm -rf $d
mkdir -p $d/n
cd $d
rawdbfs -e db n
ls n
echo 'first

record, with an empty line' > n/new
echo second > n/new
cp /bin/test/mkimg.wasm n/new
ls n
cat n/0
cmp /bin/test/mkimg.wasm n/2 && echo same binary
echo changed > n/1
rm n/0
ls n
unmount n
echo reloaded
rawdbfs db n
ls n
cat n/0
cmp /bin/test/mkimg.wasm n/1 && echo same binary
rm n/1
unmount n
echo -x
rawdbfs -x -u 'echo changed' db n
ls n
cat n/stats
echo 41 > n/index
cat n/index
cat n/index
echo more > n/new
cat n/0 n/1
cat n/stats
unmount n
echo -l
rawdbfs -l db n
{echo held; sleep 3} > n/0 &
while {! ~ `{cat n/0} held} {sleep 0}
echo other > n/0
wait
echo again > n/0
cat n/0
unmount n
echo errors
rawdbfs nodb n
echo junk > junk
rawdbfs junk n
rawdbfs -e db n
echo x > n/stats
echo x > n/nosuch
mkdir n/dir
unmount n
rawdbfs
cd /
rm -rf $d
