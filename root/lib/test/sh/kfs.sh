# disk/kfs and disk/kfscmd (kfs(4), kfscmd(8)): make a file system in an
# image, use it, check it through the console, serve it again, read only,
# and after an unclean shutdown.
d=/tmp/sh-kfs
rm -rf $d
mkdir -p $d/n
cd $d
# #s is shared by everything running: a console name of this run's own.
c=t`{cat /dev/pid}
# 512-byte blocks hold 504 bytes and 126 block numbers, so a file of
# 90,000 bytes needs the double indirect block. It is made here, not
# copied from /bin, so the output does not change when a command does.
zeros 512 400 > kfs.file
zeros -v 5a 1000 90 > big
echo ream
mount -c {disk/kfs -r -b 512 -n $c kfs.file} n
mkdir n/adm n/dir
echo '-1:adm:adm:
0:none:adm:
9999:noworld::
100:glenda:glenda:
101:dev:glenda:glenda' > n/adm/users
disk/kfscmd -n $c users
echo hello > n/dir/hello
cp big n/ls
chmod 644 n/ls
cmp big n/ls && echo same
ls n n/dir
echo more >> n/dir/hello
cat n/dir/hello
mv n/dir/hello n/dir/greeting
ls n/dir
rm n/dir
rm n/dir/greeting
rm n/dir
ls n
echo x > n/a-name-much-longer-than-27-bytes
mkdir n/adm
disk/kfscmd -n $c check
disk/kfscmd -n $c 'check Pq'
disk/kfscmd -n $c bogus
disk/kfscmd -n $c 'check z'
disk/kfscmd -n $c allowoff allow sync
unmount n
echo again
mount -c {disk/kfs -n $c kfs.file} n
cmp big n/ls && echo same
ls -l n | sed 's/^([^ ]+) . [0-9]+ [^ ]+ [^ ]+ +([0-9]+) .* ([^ ]+)$/\1 \2 \3/'
rm n/ls
disk/kfscmd -n $c 'check q'
unmount n
echo read only
mount -c {disk/kfs -R kfs.file} n
wc -l n/adm/users
echo x > n/new
unmount n
echo unclean
# fsok, the superblock's fifth word, cleared as a crash leaves it.
zeros 1 4 | dd -of kfs.file -bs 1 -oseek 528 >[2] /dev/null
mount -c {disk/kfs kfs.file} n
ls n
unmount n
echo errors
disk/kfs nosuch
zeros 512 64 > junk
disk/kfs junk
disk/kfs -r -b 1000 junk
disk/kfs
disk/kfscmd -n no.such.kfs check
cd /
rm -rf $d
