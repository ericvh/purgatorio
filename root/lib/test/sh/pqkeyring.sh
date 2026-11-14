# The kernel's own mount (mount without -k, and no factotum) proves with
# the authinfo in /usr/<user>/keyring/default. With a hybrid Ed25519 +
# ML-DSA-65 authinfo there it makes a hybrid proof, which an export that
# requires hybrid proofs (-C mldsa65) takes; with an Ed25519 one that
# export refuses it. The scratch /usr is bound after the root, so the
# tree itself is not touched.
load std
d=/tmp/sh-pqkeyring
rm -rf $d
u=`{cat /dev/user}
mkdir -p $d/lib/keys $d/root/usr/$u/keyring $d/n
auth/createsignerkey -a hybrid -f $d/hsk hybrid > $d/lib/keys/signers
auth/createsignerkey -f $d/esk classic >> $d/lib/keys/signers
auth/mkauthinfo -a hybrid -f $d/hsk $u $d/hinfo
auth/mkauthinfo -f $d/esk $u $d/einfo
auth/mkauthinfo -f $d/hsk 127.0.0.1 $d/srv
bind $d/lib/keys /lib/keys
bind -a $d/root /
styxlisten tcp!127.0.0.1!0 export -C mldsa65 -k $d/srv / >[2] $d/ql &
qpid=$apid
styxlisten tcp!127.0.0.1!0 export -k $d/srv / >[2] $d/hl &
hpid=$apid
for f in ql hl {while {! test -s $d/$f} {sleep 0}}
q=tcp!`{sed 's/styxlisten: //' $d/ql}
h=tcp!`{sed 's/styxlisten: //' $d/hl}
cp $d/hinfo /usr/$u/keyring/default
mount $q $d/n && echo hybrid keyring && unmount $d/n
mount $h $d/n && echo hybrid keyring, any server && unmount $d/n
cp $d/einfo /usr/$u/keyring/default
mount $q $d/n
mount $h $d/n && echo ed25519 keyring && unmount $d/n
kill $qpid $hpid
unmount $d/root /
unmount $d/lib/keys /lib/keys
rm -rf $d
