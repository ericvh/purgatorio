# Hybrid module signatures (docs/signing.md, "Hybrid signatures"). A
# hybrid signer certifies a builder's hybrid key; modules are signed with
# it through factotum (proto=hybrid role=modsign) and from key files, and
# checked with auth/verify -a hybrid. Under `require hybrid` only a
# hybrid signature that a listed hybrid id vouches for counts: an Ed25519
# signature, even by a listed key, is refused. The hybrid id listed
# without its mldsa65: marker trusts nothing. A module signed both ways
# runs under either policy.
load std
d=/tmp/sh-pqmodsig
rm -rf $d
mkdir -p $d/lib/keys $d/bin /mnt/factotum
auth/createsignerkey -a hybrid -f $d/hroot release > $d/hline
cp $d/hline $d/lib/keys/modules
auth/createsignerkey -f $d/eroot classic >> $d/lib/keys/modules
auth/createsignerkey -a hybrid -f $d/hkey bare > $d/bare
auth/mkauthinfo -a hybrid -f $d/hroot builder $d/hinfo
bind $d/lib /lib
for i in hybrid classic both bare {cp /bin/echo.wasm $d/bin/$i.wasm}

# factotum holds only the hybrid key: auth/sign -a hybrid asks for it,
# and plain auth/sign falls back to it when no Ed25519 key is held.
auth/factotum $d/hinfo
auth/sign -a hybrid $d/bin/hybrid.wasm
auth/sign -a ed25519 $d/bin/classic.wasm
auth/sign $d/bin/both.wasm
unmount /mnt/factotum
auth/sign -k $d/eroot $d/bin/classic.wasm
auth/sign -k $d/eroot $d/bin/both.wasm
auth/sign -a hybrid -k $d/eroot $d/bin/bare.wasm
auth/sign -k $d/hkey $d/bin/bare.wasm

fn show {
	sed 's/^signer mldsa65:[0-9a-f]* /signer hybrid /; s/^signer [0-9a-f]* /signer ed25519 /; s/^digest .*/digest ok/'
}
for i in hybrid classic both bare {
	echo verify $i
	auth/verify -m -a hybrid $d/bin/$i.wasm >[2=1] | show
}
auth/verify -m $d/bin/both.wasm | grep -c ' trusted'

echo require hybrid
auth/modpolicy -a hybrid require $d/bin/hybrid.wasm a hybrid signature runs
auth/modpolicy -a hybrid require $d/bin/both.wasm and one beside an Ed25519 signature
auth/modpolicy -a hybrid require $d/bin/classic.wasm not reached
auth/modpolicy -a hybrid require $d/bin/bare.wasm not reached
auth/modpolicy require $d/bin/classic.wasm an Ed25519 policy takes Ed25519 signatures
auth/modpolicy require $d/bin/both.wasm and either kind

echo listed
cat $d/bare >> $d/lib/keys/modules
auth/modpolicy -a hybrid require $d/bin/bare.wasm a listed hybrid key runs
echo confused
sed 's/^mldsa65://' $d/lib/keys/modules > $d/confused
cp $d/confused $d/lib/keys/modules
auth/modpolicy -a hybrid require $d/bin/hybrid.wasm not reached
auth/modpolicy require $d/bin/classic.wasm Ed25519 still

echo revoked
cp $d/hline $d/lib/keys/modules
auth/getpk $d/hroot > $d/lib/keys/revoked
auth/modpolicy -a hybrid require $d/bin/hybrid.wasm not reached
rm $d/lib/keys/revoked
unmount $d/lib /lib
rm -rf $d
