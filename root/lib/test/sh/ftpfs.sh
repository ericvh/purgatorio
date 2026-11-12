# ftpfs against /bin/test/ftpd on loopback: passive and active data
# connections, the home directory and -/, reads, writes, mkdir, rm,
# renames and the listing cache.
load std
d=/tmp/sh-ftpfs
rm -rf $d
mkdir -p $d/srv/home/sub $d/m
cd $d
echo 'in home' > srv/home/a.txt
echo 'deeper' > srv/home/sub/b
echo 'at the top' > srv/top
chmod 640 srv/home/a.txt
/bin/test/ftpd -h /home -u bob:secret srv >[2] el &
fpid=$apid
while {! test -s el} {sleep 0}
addr=`{sed 's/ftpd: //' el}
echo passive
ftpfs -q -m m -a me@here tcp!$addr
ls m m/sub
cat m/a.txt m/sub/b
fs ls m | sed 's/ [A-Z][a-z][a-z] [ 0-9][0-9] [ 0-9:][0-9:][0-9:][0-9:][0-9] / /'
echo writes
echo new file > m/new
cat m/new srv/home/new
cp /lib/test/sh/fs.sh m/sub/fs.sh
cmp /lib/test/sh/fs.sh srv/home/sub/fs.sh && echo same
echo more >> m/new
cat srv/home/new
mkdir m/dir
mv m/new m/renamed
ls m
cat srv/home/renamed
mv m/renamed m/dir/new
cat srv/home/dir/new
rm m/dir
rm m/dir/new
rm m/dir
ls srv/home
echo cache
ls m
echo 'made behind its back' > srv/home/sneaky
ls m
ls m/.flush.ftpfs
ls m
unmount m
echo root
ftpfs -q -/ -p -m m -a me@here tcp!$addr
ls m
cat m/top m/home/sneaky
echo active write > m/home/act
cat srv/home/act
unmount m
echo login
echo 'bob
secret' | ftpfs -q -m m tcp!$addr
cat m/a.txt
unmount m
echo errors
echo 'bob
wrong' | ftpfs -q -m m tcp!$addr
ftpfs -z
kill $fpid
cd /
rm -rf $d
