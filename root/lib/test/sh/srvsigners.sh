# Signers for servers only (docs/auth.md, "Signers for servers"). With one
# /lib/keys/signers list, a user certificate whose name is a host's
# proves that host under mount -S. Once /lib/keys/serversigners is there,
# only its signers count for servers: the user certificate is refused and
# the host's own is taken; an empty one takes no certificate. Then
# factotum picks the key kept for role=server, not its first key.
load std
d=/tmp/sh-srvsigners
rm -rf $d
mkdir -p $d/n $d/keys
cp /lib/test/keys/users $d/keys/users
auth/createsignerkey -f $d/usk users > $d/keys/signers
auth/createsignerkey -f $d/hsk hosts > $d/hosts
auth/mkauthinfo -f $d/usk 127.0.0.1 $d/user
auth/mkauthinfo -f $d/hsk 127.0.0.1 $d/host
auth/mkauthinfo -f $d/usk bob $d/bob
bind $d/keys /lib/keys
styxlisten tcp!127.0.0.1!0 export -k $d/user / >[2] $d/ul &
upid=$apid
styxlisten tcp!127.0.0.1!0 export -k $d/host / >[2] $d/hl &
hpid=$apid
for f in ul hl {while {! test -s $d/$f} {sleep 0}}
u=tcp!`{sed 's/styxlisten: //' $d/ul}
h=tcp!`{sed 's/styxlisten: //' $d/hl}
fn try {
	mount -S $u $d/n && echo user certificate taken && unmount $d/n
	mount -S $h $d/n && echo host certificate taken && unmount $d/n
}
echo one list
try
echo server signers
cp $d/hosts $d/keys/serversigners
try
echo none
echo '# no signer' > $d/keys/serversigners
try
kill $upid $hpid

echo factotum
cp $d/hosts $d/keys/serversigners
mkdir -p /mnt/factotum
auth/factotum -s $d/host $d/bob
sed 's/ !seed.*//' /mnt/factotum/ctl | sed 's/ cert=.*//'
styxlisten tcp!127.0.0.1!0 export / >[2] $d/fl &
fpid=$apid
while {! test -s $d/fl} {sleep 0}
f=tcp!`{sed 's/styxlisten: //' $d/fl}
mount -S $f $d/n && echo server key chosen && unmount $d/n
kill $fpid
unmount /mnt/factotum
rm -rf $d
