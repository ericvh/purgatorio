# bind and unmount: replace, before and after unions, and create.
d=/tmp/sh-bind
rm -rf $d
mkdir -p $d/a $d/b $d/c
echo 1 > $d/a/one
echo 2 > $d/b/two
echo 3 > $d/c/three
bind -b $d/a $d/b
ls -p $d/b
bind -a $d/c $d/b
ls -p $d/b
unmount $d/c $d/b
ls -p $d/b
unmount $d/a $d/b
ls -p $d/b
bind $d/a $d/b
ls -p $d/b
unmount $d/b
bind -bc $d/a $d/b
echo new > $d/b/created
ls -p $d/a
unmount $d/b
rm -r $d
