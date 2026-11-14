# Exit statuses of commands for a bad command line and for a file they
# cannot open, as Inferno's commands give them (issue #121). Most print
# their usage alone and raise fail:usage, as arg->usage does; the rest
# are as their Inferno sources have them.
load std
d=/tmp/sh-statuses
rm -rf $d
mkdir $d
m=$d/missing

fn st {
	$* < /dev/null
	echo '	'$1 $status
}

# usage: ..., fail:usage.
st wc -Z
st sort -Z
st ls -Z
st cmp $m
st mv $m
st sleep x
st sleep
st stack
st nsbuild a b

# fail: usage, with a space, and some print Usage:.
st rm -Z
st tr -Z
st tokenize
st logfile
st install/inst

# Other statuses for a bad command line.
st crypt -Z
st mk -Z
st stream
st install/info
st uudecode -Z
st unicode
st install/create
st install/wrap2list -Z
st install/ckproto
st unmount
st xargs
st ramfile

# Nothing to make is not an error.
st mkdir

# A file that cannot be opened.
st wc $m
st md5sum $m
st sha1sum $m
st sum $m
st du $m
st tail $m
st xd $m
st sort $m
st uniq $m
st strings $m
st freq $m
st fmt $m
st gzip $m
st units $m
st tcs $m
st p $m
st mc $m
st stream $m
st auth/getpk $m
st auth/ai2key $m
st disk/mbr $m
st disk/fdisk $m
st wasmdump $m
st xargs $d/nosuchcmd
st ns 999999
st stack 999999 >[2] /dev/null
st unmount $m >[2] /dev/null

# File tools.
st cp $m $d/x
st mv $m $d/x
st chmod 777 $m
st chgrp sys $m
st mkdir $d
st mkdir $m/a
st tee $m/a
st bind $m $d
st comm $m $m

rm -r $d
