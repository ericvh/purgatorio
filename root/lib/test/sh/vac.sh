# vacput, vacget and vacfs against test/netsrv venti, a Venti server on
# a loopback port that keeps its blocks in memory.
load std
d=/tmp/sh-vac
rm -rf $d
mkdir -p $d/src/sub/deeper $d/out $d/m
cd $d
echo hello > src/a
echo 'in sub' > src/sub/b
echo 'deeper still' > src/sub/deeper/c
cp /lib/test/sh/fs.sh src/sub/fs.sh
touch src/empty
chmod 640 src/a src/sub/b
s=`{test/netsrv venti}
v=tcp!127.0.0.1!^${hd $s}

echo put
score=`{vacput -a $v src}
echo $score | sed 's/[0-9a-f]/x/g'
vacput -v -a $v src | sed 's/^vac:.*/vac:.../'

echo list
vacget -t -a $v $score | sort

echo get
cd out
vacget -a $v $score
cd ..
cmp src/sub/fs.sh out/src/sub/fs.sh && echo same
cat out/src/a out/src/sub/b out/src/sub/deeper/c
ls out/src out/src/sub out/src/sub/deeper
# The mode a new file gets depends on the file system under /tmp.
ls -l out/src/empty | sed 's/^[^ ]* [^ ]* [^ ]* [^ ]* [^ ]* ([0-9]*) .*/empty \1/'

echo small blocks, two paths, a name, an owner
score2=`{vacput -b 512 -n other -u bob -g sys -a $v src/sub /lib/test/sh/fs.sh}
h2=`{echo $score2 | sed 's/vac://'}
cd out
rm -rf src
vacget -v -a $v $h2 | sort
cmp sub/fs.sh /lib/test/sh/fs.sh && cmp fs.sh /lib/test/sh/fs.sh && echo same
cd ..
vacput -b 100 -a $v src

echo include and exclude
echo 'src
src/sub
src/sub/b' | vacput -i -a $v src > score
vacget -t -a $v `{cat score}
echo src/sub | vacput -x -a $v src > score
vacget -t -a $v `{cat score}

echo vacfs
mount {vacfs -a $v $score} m
ls m m/src m/src/sub
cat m/src/a m/src/sub/deeper/c
cmp m/src/sub/fs.sh src/sub/fs.sh && echo same
ls -l m/src/a | sed 's/^([^ ]*) [^ ]* [^ ]* [^ ]* [^ ]* ([0-9]*) .*/\1 \2/'
echo x > m/src/a
cat m/nonexistent
unmount m
mount {vacfs -a $v $score2} m
ls -l m/sub/b | sed 's/^([^ ]*) [^ ]* [^ ]* ([^ ]*) ([^ ]*) ([0-9]*) .*/\1 \2 \3 \4/'
cat m/sub/b
unmount m

echo any archive
mount {vacfs -p -a $v} m
ls m
cat m/^$score/src/a
cat m/$h2/sub/b
unmount m

echo errors
vacget -a $v vac:0123456789012345678901234567890123456789
vacget -a $v fossil:0123456789012345678901234567890123456789
vacget -a $v vac:xyz
vacput -i -x -a $v src
vacput -a tcp!127.0.0.1!1 src
kill ${tl $s}
cd /
rm -rf $d
