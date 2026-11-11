# bind makes the checks of Inferno's cmount: -c cannot bind a mount
# point that is a union or that was not mounted with -c, a file cannot
# be a union, and a file and a directory cannot be bound on each other.
# Each is "inconsistent mount" (issue #118).
d=/tmp/sh-bindc
rm -rf $d
mkdir -p $d/a $d/b $d/x $d/y $d/u $d/fs
touch $d/f $d/g
memfs $d/fs
bind -c $d/fs $d/y && echo a mount made with -c binds with -c
unmount $d/y
bind $d/a $d/x
bind -c $d/x $d/y
bind $d/x $d/y && echo it binds without -c
unmount $d/y
bind -a $d/b $d/u
bind -c $d/u $d/y
bind -a $d/g $d/f
bind $d/a $d/f
bind $d/g $d/f && echo a file binds on a file
unmount $d/f
unmount $d/u
unmount $d/x
unmount $d/fs
rm -rf $d
