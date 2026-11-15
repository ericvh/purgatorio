# A certificate chain made in the system: a signer certifies an
# intermediate key, which certifies the key that signs a module
# (mkauthinfo -f with an authinfo, getpk -c for its certificate). The
# module verifies as trusted through the chain, naming both keys; without
# the intermediate's certificate its signer is untrusted.
d=/tmp/sh-modchain
rm -rf $d
mkdir -p $d
auth/createsignerkey -f $d/sk rootsigner > $d/modules
auth/mkauthinfo -f $d/sk mid $d/mid.info
auth/mkauthinfo -f $d/mid.info leaf $d/leaf.info
auth/getpk -c $d/mid.info > $d/mid.cert
auth/getpk -c $d/sk >[2=1] | sed 's/^.*: //'
cp /bin/echo.wasm $d/e.wasm
cp /bin/echo.wasm $d/g.wasm
auth/sign -k $d/leaf.info -c $d/mid.cert $d/e.wasm
auth/sign -k $d/leaf.info $d/g.wasm
auth/verify -m -s $d/modules $d/e.wasm | sed 's/[0-9a-f][0-9a-f][0-9a-f]+/HEX/'
echo status $status
auth/verify -m -s $d/modules $d/g.wasm >[2=1] | sed 's/[0-9a-f][0-9a-f][0-9a-f]+/HEX/'
rm -rf $d
