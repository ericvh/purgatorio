# File commands: mkdir, ls, cp, mv, rm, cd and pwd.
d=/tmp/sh-files
rm -rf $d
mkdir -p $d/a/b
echo data > $d/a/f
ls -p $d/a
cp $d/a/f $d/g
cat $d/g
cp -r $d/a $d/c
ls $d/c
mv $d/g $d/h
ls -p $d
cd $d/a
pwd
ls
cd /
pwd
rm $d/h
rm $d/nonexistent
echo $status
rm -f $d/nonexistent
echo $status
# A name that starts with # is a device, through std's file calls too.
cat '#c/sysname' > $d/s
cmp $d/s /dev/sysname && echo cat '#c/sysname'
cd /lib
cmp '#c/sysname' /dev/sysname && echo cmp '#c/sysname' in /lib
cd /
cat '#c/nosuch'
ls '#c' | grep -s sysname && echo ls '#c'
rm -r $d
ls $d >[2] /dev/null || echo removed
