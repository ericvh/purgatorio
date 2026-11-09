# Small text tools.
d=/tmp/sh-tools
rm -rf $d
mkdir -p $d
echo hello > $d/a
echo hello > $d/b
echo world > $d/c
grep hello $d/a
grep -n hello $d/a $d/c
cmp $d/a $d/b && echo same
echo -n hello | md5sum
tail -n 1 $d/c
echo hi | tee $d/tee
echo foo | sed s/foo/bar/
echo foo foo | sed s/foo/bar/g
fc $d/a $d/copy
cat $d/copy
du $d/a
echo -n hi | xd
diff $d/a $d/c || echo differed
