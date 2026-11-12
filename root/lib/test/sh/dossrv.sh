# dossrv: FAT12, FAT16 and FAT32 images made by /bin/test/mkimg, read
# and written, with long names, then served again to see what stayed.
load std
d=/tmp/sh-dossrv
rm -rf $d
mkdir -p $d/src/Docs $d/m
cd $d
echo 'read me' > src/Docs/readme.txt
echo hello > 'src/A Long Mixed-Case Name.text'
echo locked > src/ro
chmod 444 src/ro
/bin/test/mkimg fat 12 2880 f12.img src
echo fat12
dossrv -f f12.img -m m
fs ls m | sed 's/ [A-Z][a-z][a-z] [ 0-9][0-9] [ 0-9:][0-9:][0-9:][0-9:][0-9] / /'
cat m/Docs/readme.txt 'm/a long mixed-case NAME.TEXT'
echo writes
mkdir m/new
echo one > m/new/one
echo 'a much longer name than 8.3 allows' > 'm/new/A much longer name than 8.3 allows.txt'
cp /lib/test/sh/fs.sh m/new/fs.sh
cmp /lib/test/sh/fs.sh m/new/fs.sh && echo same
echo more >> m/new/one
cat m/new/one
mv m/new/one m/new/Two
ls m/new
chmod 444 m/new/Two
echo x > m/new/Two
rm m/new/Two
chmod 666 m/new/Two
rm m/new/Two
rm m/new
rm m/ro
echo truncated > m/Docs/readme.txt
cat m/Docs/readme.txt
unmount m
echo again
dossrv -f f12.img -m m
fs ls m | sed 's/ [A-Z][a-z][a-z] [ 0-9][0-9] [ 0-9:][0-9:][0-9:][0-9:][0-9] / /'
cmp /lib/test/sh/fs.sh m/new/fs.sh && echo same
cat m/Docs/readme.txt
unmount m
for k in 16 32 {
	echo fat$k
	/bin/test/mkimg fat $k 9000 f.img src
	dossrv -f f.img -m m
	ls m m/Docs
	mkdir m/d
	cp /lib/test/sh/fs.out 'm/d/Long File Name.out'
	unmount m
	dossrv -f f.img -m m
	cmp /lib/test/sh/fs.out 'm/d/long file name.out' && echo same
	unmount m
}
echo full
/bin/test/mkimg fat 12 64 small.img
dossrv -f small.img -m m
cp /lib/test/sh/fs.sh m/a
cp /lib/test/sh/fs.out m/b
cp /bin/sh.wasm m/c
ls m
mkdir m/x
unmount m
echo errors
dossrv -f f12.img -m m
echo x > 'm/bad:name'
mkdir m/Docs
rm m/Docs
unmount m
dossrv -f nosuch -m m
echo junk > junk
dossrv -f junk -m m
dossrv -q
cd /
rm -rf $d
