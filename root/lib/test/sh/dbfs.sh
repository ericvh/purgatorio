# dbfs: the records of a text file as numbered files, and `new`.
load std
d=/tmp/sh-dbfs
rm -rf $d
mkdir -p $d/n
cd $d
echo 'first
record

second

third' > db
dbfs db n
ls n
cat n/0
cat n/2
echo changed > n/1
cat db
# new: the write makes the record, and it is listed.
echo 'a new one' > n/new
ls n
cat n/3
rm n/0
ls n
cat db
# A record cannot hold an empty line; the bad write changes nothing.
echo 'x

y' > n/2
cat n/2
# Truncating a record empties it, and an empty record is not kept.
echo -n > n/1
cat n/nosuch
rm n/new
mkdir n/dir
unmount n
cat db
# Numbers are positions: after the removal they have moved up.
dbfs db n
ls n
cat n/0
unmount n
# Read-only, a file that is not there, and -e.
dbfs -r db n
cat n/0
echo no > n/0
echo no > n/new
rm n/0
unmount n
dbfs nodb n
dbfs -e nodb n
ls n
echo made > n/new
unmount n
cat nodb
dbfs
cd /
rm -rf $d
