# Module signing through the guest factotum (docs/signing.md): factotum
# holds the builder's key and certificate and signs the module itself,
# and a second signer countersigns from a key file. A signature made
# through a certificate that has expired no longer verifies.
load std
d=/tmp/sign-factotum
rm -rf $d
mkdir -p $d /mnt/factotum
auth/createsignerkey -f $d/root release > $d/modules
auth/createsignerkey -f $d/other other > $d/others
auth/mkauthinfo -f $d/root builder $d/info
cp /bin/echo.wasm $d/hello.wasm
auth/sign $d/hello.wasm
auth/factotum $d/info
auth/sign $d/hello.wasm && echo signed
unmount /mnt/factotum
auth/sign -k $d/other $d/hello.wasm && echo countersigned
auth/verify -m -s $d/modules $d/hello.wasm | sed 's/^signer [0-9a-f]* /signer /' | sed 's/^digest .*/digest ok/'
auth/verify -m -s $d/others $d/hello.wasm | grep -c ' trusted'
auth/verify -m -s $d/modules -r $d/modules $d/hello.wasm > /dev/null
# The module still runs, and removing nothing but the signature gives the
# original: the signed file is the original and one more section.
$d/hello.wasm it runs
n=`{wc -c < /bin/echo.wasm}
dd -if $d/hello.wasm -of $d/plain.wasm -bs $n -count 1 >[2] /dev/null
cmp /bin/echo.wasm $d/plain.wasm && echo same module

auth/mkauthinfo -f $d/root -e 1 builder $d/old
cp /bin/echo.wasm $d/old.wasm
auth/sign -k $d/old $d/old.wasm
auth/verify -m -s $d/modules $d/old.wasm > /dev/null
auth/verify -m -s $d/modules -t 0 $d/old.wasm > /dev/null && echo not expired then
rm -rf $d
