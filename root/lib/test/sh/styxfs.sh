# File servers of the Styx tools: tarfs, archfs, trfs and lockfs.
d=/tmp/sh-styxfs
rm -rf $d
mkdir -p $d/src/sub $d/m $d/a $d/l $d/t
cd $d
echo hello > src/a
echo world > src/sub/b
# tarfs: a read-only view of a tar archive.
tar cf t.tar src
tarfs t.tar m
ls m m/src m/src/sub
cat m/src/sub/b
cp src/a m/src/a >[2=1]
rm m/src/a >[2=1]
tarfs -p t.tar m
ls -l m/src/a | sed 's/ .*//'
tarfs missing.tar m >[2=1]
unmount m
# archfs: an mkfs archive, header lines then data.
echo 'a 644 alice bob 5 6' > ar
echo hello >> ar
echo 'd 20000000755 alice bob 5 0' >> ar
echo 'd/e 600 alice bob 5 3' >> ar
echo hi >> ar
echo 'end of archive' >> ar
archfs -m a ar
ls -l a a/d | sed 's/ M [0-9][0-9]* / M n /'
cat a/a a/d/e
archfs -m a ar d
ls a
echo 'a 644 alice' > bad
archfs -m a bad >[2=1]
echo 'a 644 alice bob 5 6' > short
archfs -m a short >[2=1]
# trfs: spaces in names as no-break spaces.
echo 'my file' > 'l/my file'
trfs l t
ls t
cat 't/my file'
echo new > 't/x y'
ls l
# lockfs: one writer or many readers.
lockfs l
cat 'l/x y'
echo more >> 'l/x y'
cat 'l/x y'
# A reader waits while a writer has the file open.
load std
{echo one; touch $d/ready; sleep 1; echo two} > 'l/x y' &
while {! ls $d/ready >[2] /dev/null > /dev/null} {sleep 0.1}
cat 'l/x y'
wait
lockfs >[2=1]
lockfs l/x >[2=1]
cd /
unmount $d/l
unmount $d/t
unmount $d/a
rm -r $d
