# Hybrid Ed25519 + ML-DSA-65 signatures (docs/auth.md, "Post-quantum
# signatures"). A hybrid signer certifies hybrid keys, offline with
# mkauthinfo -a hybrid and online with getauthinfo -a hybrid; an export
# that trusts the signer's id takes their proofs, and proves itself with
# a hybrid server certificate. -C mldsa65 requires hybrid proofs at
# either end, and an Ed25519 key, user or server, is refused there. The
# signer's id listed without its mldsa65: marker is an Ed25519 entry and
# vouches for nothing hybrid. Ed25519 keys and signers keep working
# beside the hybrid ones.
load std
d=/tmp/sh-pqsig
rm -rf $d
mkdir -p $d/lib/keys $d/keys $d/n
u=`{cat /dev/user}
auth/createsignerkey -a hybrid -f $d/hsk hybrid > $d/lib/keys/signers
auth/createsignerkey -f $d/esk classic >> $d/lib/keys/signers
sed 's/:.*//; s/^[0-9a-f]* /ed25519 /' $d/lib/keys/signers
wc -c < $d/hsk
cat /lib/test/keys/users > $d/lib/keys/users
bind $d/lib/keys /lib/keys
auth/mkauthinfo -a hybrid -f $d/hsk $u $d/user
auth/mkauthinfo -f $d/hsk 127.0.0.1 $d/srv
auth/mkauthinfo -f $d/esk 127.0.0.1 $d/esrv
auth/mkauthinfo -f $d/esk $u $d/euser
auth/mkauthinfo -a ed25519 -f $d/hsk $u $d/no
sed 's/:.*//; s/^pgcert2 [^ ]* /pgcert2 /' $d/user
auth/getpk $d/user | sed 's/:.*//'
auth/getpk -x $d/user | wc -c
auth/verify -a hybrid $d/user | grep -v '^name' | sed 's/ mldsa65:[0-9a-f]*//; s/^expires .*/expires/'
auth/verify -a hybrid $d/euser > /dev/null
auth/countersigner -f $d/esk $d/user > /dev/null
styxlisten tcp!127.0.0.1!0 export -k $d/srv / >[2] $d/hl &
hpid=$apid
styxlisten tcp!127.0.0.1!0 export -C mldsa65 -k $d/srv / >[2] $d/ql &
qpid=$apid
styxlisten tcp!127.0.0.1!0 export -k $d/esrv / >[2] $d/el &
epid=$apid
for f in hl ql el {while {! test -s $d/$f} {sleep 0}}
h=tcp!`{sed 's/styxlisten: //' $d/hl}
q=tcp!`{sed 's/styxlisten: //' $d/ql}
e=tcp!`{sed 's/styxlisten: //' $d/el}
mount -k $d/user $h $d/n && echo hybrid user && unmount $d/n
mount -S -k $d/user $h $d/n && echo hybrid server && unmount $d/n
mount -C mldsa65 -k $d/user $q $d/n && echo hybrid required && unmount $d/n
mount -C x25519mlkem768/mldsa65 -k $d/user $q $d/n && echo all post-quantum && unmount $d/n
mount -k $d/euser $h $d/n && echo classic user && unmount $d/n
mount $h $d/n && echo test key && unmount $d/n
mount -k $d/euser $q $d/n
mount $q $d/n
mount -S -k $d/euser $e $d/n && echo classic server && unmount $d/n
mount -C mldsa65 -k $d/user $e $d/n
mount -C mldsa65 -k $d/euser $e $d/n
sed 's/^mldsa65://' $d/lib/keys/signers > $d/confused
cp $d/confused $d/lib/keys/signers
mount -k $d/user $h $d/n
mount -k $d/euser $h $d/n && echo classic still && unmount $d/n
kill $hpid $qpid $epid
echo factotum
auth/createsignerkey -a hybrid -f $d/hsk hybrid > $d/lib/keys/signers
auth/mkauthinfo -f $d/hsk $u $d/user
auth/mkauthinfo -f $d/hsk 127.0.0.1 $d/srv
styxlisten tcp!127.0.0.1!0 export -C mldsa65 -k $d/srv / >[2] $d/fl &
fpid=$apid
while {! test -s $d/fl} {sleep 0}
f=tcp!`{sed 's/styxlisten: //' $d/fl}
mkdir -p /mnt/factotum
auth/factotum $d/user
sed 's/ user=[^ ]*//; s/cert=.*/cert=.../' /mnt/factotum/ctl
mount -S -C mldsa65 $f $d/n && echo factotum hybrid && unmount $d/n
unmount /mnt/factotum
kill $fpid
echo signer
echo master | auth/keyfs -m $d/keys -C 8,1 $d/keydb
echo secret | auth/changelogin -m $d/keys -C 8,1 $u
styxlisten tcp!127.0.0.1!0 /bin/auth/signer.wasm -f $d/hsk -m $d/keys >[2] $d/sl &
spid=$apid
while {! test -s $d/sl} {sleep 0}
signer=tcp!`{sed 's/styxlisten: //' $d/sl}
echo secret | getauthinfo -a hybrid -s $signer $d/info && sed 's/:.*//; s/^pgcert2 [^ ]* /pgcert2 /' $d/info
echo secret | getauthinfo -s $signer $d/info2
auth/verify $d/info > /dev/null && echo verified
kill $spid
unmount $d/keys
unmount $d/lib/keys /lib/keys
rm -rf $d
