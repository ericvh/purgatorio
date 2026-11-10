# Module signing (docs/signing.md). A module is signed inside the system,
# through the agent #¤ (by a key the trusted signer certified) and from
# key files, and checked with auth/verify.
# Under the policy `require`, set by auth/modpolicy with /lib/keys bound
# from a scratch directory, the signed module runs; an unsigned one, one
# with another module's signature, one by an untrusted signer and one by
# a revoked signer are refused. A signed shell under `require` cannot
# start unsigned commands. Under `warn` an unsigned module runs after a
# warning.
load std
d=/tmp/sh-modsig
rm -rf $d
mkdir -p $d/lib/keys $d/bin
auth/createsignerkey -f $d/root release > $d/lib/keys/modules
auth/mkauthinfo -f $d/root builder $d/info
auth/createsignerkey -f $d/key2 stranger > $d/stranger
bind $d/lib /lib
for i in hello plain other both {cp /bin/echo.wasm $d/bin/$i.wasm}
cp /bin/sh.wasm $d/bin/sh.wasm

# Sign through the agent: the builder's key and certificate are in #¤,
# and sign never reads the key.
grep '^key ' $d/info > '#¤/ctl'
echo cert `{grep '^pgcert1 ' $d/info} > '#¤/ctl'
auth/sign $d/bin/hello.wasm
echo delkey > '#¤/ctl'
auth/sign $d/bin/plain.wasm
auth/sign -k $d/info $d/bin/sh.wasm
auth/sign -k $d/key2 $d/bin/other.wasm
auth/sign -k $d/key2 $d/bin/both.wasm
auth/sign -k $d/root -o $d/bin/both.wasm $d/bin/both.wasm
# The signed module is the original with one section appended. Move that
# section onto another module to get a signature that does not match.
n=`{wc -c < /bin/echo.wasm}
dd -if $d/bin/hello.wasm -of $d/sig -ibs 1 -iseek $n >[2] /dev/null
cat /bin/cat.wasm $d/sig > $d/bin/changed.wasm

for i in hello plain other both changed {
	echo verify $i
	auth/verify -m $d/bin/$i.wasm >[2=1] | sed 's/^signer [0-9a-f]* /signer /' | sed 's/^digest .*/digest ok/'
}

echo require
auth/modpolicy require $d/bin/hello.wasm a signed module runs
auth/modpolicy require $d/bin/both.wasm one trusted signer is enough
auth/modpolicy require $d/bin/plain.wasm not reached
auth/modpolicy require $d/bin/other.wasm not reached
auth/modpolicy require $d/bin/changed.wasm /dev/null
auth/modpolicy require echo not reached
auth/modpolicy require $d/bin/sh.wasm -c $d'/bin/hello.wasm a signed shell starts signed modules; '$d'/bin/plain.wasm not reached; ls /; load add; echo not reached'

echo revoked
auth/getpk $d/root > $d/lib/keys/revoked
auth/modpolicy require $d/bin/hello.wasm not reached
auth/verify -m $d/bin/both.wasm >[2=1] | sed 's/^signer [0-9a-f]* /signer /' | sed 's/^digest .*/digest ok/'
echo junk > $d/lib/keys/revoked
auth/modpolicy require $d/bin/hello.wasm not reached
rm $d/lib/keys/revoked
auth/modpolicy require $d/bin/hello.wasm trusted again

echo warn
auth/modpolicy warn $d/bin/plain.wasm an unsigned module runs after a warning
auth/modpolicy warn $d/bin/hello.wasm and a signed one without
echo unsigned modules still run here
unmount $d/lib /lib
rm -rf $d
