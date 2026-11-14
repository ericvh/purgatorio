# Hybrid Ed25519 + ML-DSA-65 signatures on rstyx (docs/rcmd.md,
# "Post-quantum signatures"). rcmd -e mldsa65 (cpu -C mldsa65) requires
# the server to prove itself with a hybrid key or certificate, as -S
# does; rstyxd -C mldsa65 requires a hybrid proof of every client and
# refuses an Ed25519 one with the error an export gives. Without them a
# hybrid user and an Ed25519 one both get in. Both ends trust a hybrid
# signer and the test key.
load std
d=/tmp/sh-rcmdpqsig
rm -rf $d
mkdir -p $d/lib/keys
u=`{cat /dev/user}
auth/createsignerkey -a hybrid -f $d/hsk hybrid > $d/lib/keys/signers
cp /lib/test/keys/users /lib/test/keys/servers $d/lib/keys
auth/mkauthinfo -a hybrid -f $d/hsk $u $d/user
auth/mkauthinfo -a hybrid -f $d/hsk 127.0.0.1 $d/srv
bind $d/lib/keys /lib/keys
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm -k $d/srv >[2] $d/l &
lpid=$apid
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm -C mldsa65 -k $d/srv >[2] $d/q &
qpid=$apid
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm >[2] $d/e &
epid=$apid
for f in l q e {while {! test -s $d/$f} {sleep 0}}
l=tcp!`{sed 's/styxlisten: //' $d/l}
q=tcp!`{sed 's/styxlisten: //' $d/q}
e=tcp!`{sed 's/styxlisten: //' $d/e}
rcmd -k $d/user $l echo hybrid user </dev/null
rcmd $l echo test key </dev/null
rcmd -e mldsa65 -k $d/user $l echo hybrid both ways </dev/null
rcmd -k $d/user -e x25519mlkem768/mldsa65 $q echo all post-quantum </dev/null
cpu -C mldsa65 -k $d/user $q echo cpu hybrid </dev/null
rcmd $q echo no </dev/null
echo status $status
rcmd -e mldsa65 -k $d/user $e echo no </dev/null
echo status $status
rcmd -A -e mldsa65 $l echo no </dev/null
kill $lpid $qpid $epid
auxi/rstyxd -A -C mldsa65 </dev/null
unmount /lib/keys
rm -rf $d
