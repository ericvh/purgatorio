# disk/mkdisk and disk/chkdisk on purgfs image files (docs/fs.md).
load std
d=/tmp/disktest
rm -rf $d
mkdir -p $d/src/sub
echo hello > $d/src/a
echo inner > $d/src/sub/b
chmod 600 $d/src/a

# An empty file system, then one holding a copy of $d/src.
disk/mkdisk -s 256k -b 1k $d/img
disk/chkdisk -v $d/img
emuinit disk/chkdisk $d/img && echo emuinit disk/chkdisk
disk/mkdisk -s 256k -r $d/src $d/img
disk/chkdisk -v $d/img
echo status $status

# A damaged entry: the root's first block is block 34, and "a" is in its
# first slot, so its name is at byte 34*4096+116.
echo -n X | dd -of $d/img -bs 1 -oseek 139380 >[2] /dev/null
disk/chkdisk $d/img
echo status $status

# A damaged superblock is not a file system.
echo -n X | dd -of $d/img -bs 1 -oseek 100 >[2] /dev/null
disk/chkdisk $d/img
echo status $status

# Usage and missing files.
disk/mkdisk $d/none
echo status $status
disk/mkdisk -s 1k $d/tiny
echo status $status
rm -rf $d
