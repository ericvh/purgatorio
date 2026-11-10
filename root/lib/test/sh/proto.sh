# Prototypes: install/mkproto, install/proto2list, install/ckproto, and
# disk/mkfs and disk/mkext on a small tree in /tmp.
d=/tmp/prototest
rm -rf $d
mkdir -p $d/src/lib/sub $d/src/bin
cd $d/src
echo one > bin/one
echo two two > bin/two
echo lib > lib/l.b
echo gen > lib/l.dis
echo deep > lib/sub/deep
echo 'with space' > 'lib/a b'
chmod 664 bin/one bin/two lib/l.b lib/l.dis lib/sub/deep 'lib/a b'
chmod 775 . bin lib lib/sub
touch -t 1000000 bin/one bin/two lib/l.b lib/l.dis lib/sub/deep 'lib/a b'
touch -t 2000000 . bin lib lib/sub
cd $d

install/mkproto src
install/mkproto -lib src
install/mkproto src/bin src/lib/sub
install/mkproto src/nosuch

echo 'tmp
	prototest
		src
			bin
				*
			lib
				%
				sub
					+
' > p1
# The times of /tmp and /tmp/prototest vary; leave them out.
install/proto2list p1 | grep src
echo 'tmp
	prototest
		src
			bin
				one
				renamed - - - /tmp/prototest/src/bin/two
				missing
' > p2
install/proto2list p2 | grep src

echo 'bin
	*
lib
	l.b
' > p3
install/ckproto -r $d/src p3
install/ckproto -o -r $d/src p3
install/ckproto -v -r $d/src p3

# mkfs -a and mkext: an archive of the tree and back.
echo 'bin
	+
lib
	l.b 600
	sub
		deep - - - /lib/l.b
' > p4
disk/mkfs -a -s $d/src -U ann -G staff p4 > arch
disk/mkfs -a -x -s $d/src p4
disk/mkext -h -d /x < arch
mkdir out
disk/mkext -T -d $d/out < arch
ls out out/bin out/lib out/lib/sub
cat out/lib/sub/deep
mkdir part
disk/mkext -v -d $d/part /lib/sub < arch
ls part part/lib part/lib/sub
echo 'bad header' | disk/mkext -d $d/part

# mkfs -d copies what is out of date into a tree.
mkdir dest
disk/mkfs -v -s $d/src -d $d/dest p4
cat dest/bin/two
disk/mkfs -v -s $d/src -d $d/dest p4
disk/mkfs -r -v -s $d/src -d $d/dest p4
disk/mkfs -s $d/src -d $d/dest nosuchproto
disk/mkfs
cd /
rm -rf $d
