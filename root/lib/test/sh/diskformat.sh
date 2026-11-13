# disk/format (prep(8)): FAT file systems on images, read back by dossrv.
d=/tmp/sh-diskformat
rm -rf $d
mkdir -p $d/m
cd $d
echo floppy
echo 'loader' > 9load
zeros 512 4 > f.img
disk/format -d -l purg f.img 9load /lib/test/sh/fs.sh
ls -l f.img | sed 's/.* ([0-9]+) [A-Z].*/\1/'
dd -if f.img -bs 1 -iseek 43 -count 11 >[2] /dev/null
echo
dd -if f.img -bs 1 -iseek 54 -count 8 >[2] /dev/null
echo
dossrv -f f.img -m m
ls m
cmp m/fs.sh /lib/test/sh/fs.sh && echo same
ls -l m/9load | sed 's/ .*//'
unmount m
echo hard
zeros 512 4200 > h.img
disk/format -d -t hard -r 2 h.img
dd -if h.img -bs 1 -iseek 13 -count 4 >[2] /dev/null | xd
dd -if h.img -bs 1 -iseek 54 -count 8 >[2] /dev/null
echo
dossrv -f h.img -m m
cp /lib/test/sh/fs.out m/fs.out
unmount m
dossrv -f h.img -m m
cmp m/fs.out /lib/test/sh/fs.out && echo same
unmount m
echo boot
echo 'BOOT CODE that is long enough to cover the parameter block of the FAT, and more' > boot
disk/format -b boot h.img
dd -if h.img -bs 1 -count 3 >[2] /dev/null
echo
dd -if h.img -bs 1 -iseek 62 -count 6 >[2] /dev/null
echo
dossrv -f h.img -m m
ls m
unmount m
echo errors
cp f.img 9fat
disk/format -d 9fat
disk/format -d -t 8inch f.img
disk/format -d -c 1 -t hard h.img
dd -if h.img -bs 1 -iseek 54 -count 8 >[2] /dev/null
echo
disk/format -d nosuch
disk/format
cd /
rm -rf $d
