# disk/fdisk, disk/prep and disk/mbr on image files (sd(3) partitions
# are tested by the kernel, which has disks).
d=/tmp/diskpart
rm -rf $d
mkdir $d
cd $d
zeros 512 2048 > img

# fdisk: an empty table, then one Plan 9 partition made by -a.
disk/fdisk -s 512 -p img
disk/fdisk -p img
disk/fdisk -s 512 -baw img
disk/fdisk -s 512 -p img
echo 'p
d p1
a p2 63 100
a p1 100 .+50%
a p3 100 200
a p3 $ $
t p2 dos
A p2
x
p
P
e
q
w
q' | disk/fdisk -s 512 img
disk/fdisk -s 512 -p img
echo 'a p4 1 2
t p4
q
q' | disk/fdisk -r -s 512 img
dd -if img -bs 2 -iseek 255 -count 1 >[2] /dev/null | xd

# fdisk: secondary partitions, two chains, then a chain broken by a
# primary partition, and one chain too many.
zeros 512 2048 > img3
echo 'a p1 63 500
a s1 500 900
a s2 900 1400
a s3 1500 2048
p
e
w
q' | disk/fdisk -s 512 img3
disk/fdisk -s 512 -p img3
echo 'p
d s2
a p2 900 1400
e
w
q' | disk/fdisk -s 512 img3
disk/fdisk -s 512 -p img3
echo 'a p3 1400 1450
a p4 1450 1500
w
q
q' | disk/fdisk -s 512 img3

# mbr: the default boot code, and -9.
disk/mbr img >[2] /dev/null
dd -if img -bs 1 -iseek 22 -count 19 >[2] /dev/null
echo
disk/fdisk -s 512 -p img
disk/mbr -9 img >[2] /dev/null
disk/fdisk -s 512 -p img
echo hello > code
disk/mbr -m code img >[2] /dev/null
dd -if img -bs 1 -count 6 >[2] /dev/null

# prep: -a, the table in sector 1, and the editor with prompts.
zeros 512 2048 > img2
disk/prep -s 512 -bw -a 9fat -a fs -a nvram img2
dd -if img2 -bs 512 -iseek 1 -count 1 >[2] /dev/null | tr -d '\0'
disk/prep -s 512 -p img2
echo 'p
d fs
. 207
a fs
.
.+100
a other . $
a bad name 1
a fs 1 2
P
w
q' | disk/prep -s 512 img2
disk/prep -s 512 -p img2
disk/prep -s 512 -a nosuch img2
echo $status
cd /
rm -rf $d
