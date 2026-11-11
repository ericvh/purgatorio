# ns after binds from inside a mounted tree (issue #103). A directory of
# a mounted tree bound elsewhere is a bind of its name; only the mount
# itself is a mount. Lines come in the order the binds and mounts were
# made, with their flags, and the underlying directory of a union is not
# listed.
d=/tmp/sh-nsbind
rm -rf $d
mkdir -p $d/u $d/n $d/b $d/c $d/fs
memfs $d/fs
mkdir $d/fs/sub
echo in sub > $d/fs/sub/f
bind $d/fs/sub $d/u
bind -a $d/u $d/n
bind -bc $d/fs $d/b
bind -c $d/c $d/c
bind -b $d/n $d/c
ns | grep $d
cat $d/n/f
unmount $d/c
unmount $d/b
unmount $d/n
unmount $d/u
unmount $d/fs
ns | grep -c $d
rm -rf $d
