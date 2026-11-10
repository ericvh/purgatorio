# wasmdump and wasmdep (issue #40): what is in a module and what it needs.
# /lib/test/tiny.wasm is a fixed module with one of everything (the
# sample of appl/lib/wasmobj's tests), so its whole dump is stable. For
# the modules the build makes, only what does not move with the compiler
# is shown.
d=/tmp/sh-wasmtools
rm -rf $d
mkdir $d

echo dump
wasmdump /lib/test/tiny.wasm
echo functions
wasmdump -f /lib/test/tiny.wasm
echo parts
wasmdump -ie /lib/test/tiny.wasm /lib/test/tiny.wasm | grep -v '^	'

echo built modules
wasmdump -e /bin/lib/add.wasm | sed 's/func [0-9]+ add/func N add/'
wasmdump -h /bin/lib/add.wasm | sed 's/^(size|digest|abi) .*/\1 ok/'
# A stripped module is named from the .names file beside it, or from its
# exports where that is left out (the kernel and the probe).
wasmdump -f /bin/lib/add.wasm | grep ' add$' | sed 's/^func [0-9]+ [0-9]+/func N N/'

echo dep
wasmdep /lib/test/tiny.wasm
wasmdep /bin/lib/add.wasm | sed 's/^abi .*/abi ok/'
wasmdep /bin/echo.wasm | grep ' (fd_write|exits)$'
wasmdep /lib/test/tiny.wasm /bin/lib/add.wasm | grep '^module'

echo signed
# A bare key: the public test key, so the signer is the same everywhere.
auth/sign -k /lib/test/auth.key -o $d/key.wasm /bin/echo.wasm
wasmdump -c $d/key.wasm | grep -v 'purgatorio.abi'
# A certificate: the names in the chain follow the key.
auth/createsignerkey -f $d/root release > $d/modules
auth/mkauthinfo -f $d/root builder $d/info
auth/sign -k $d/info -o $d/cert.wasm /bin/echo.wasm
wasmdump -c $d/cert.wasm | grep signer | sed 's/signer [0-9a-f]+/signer KEY/'
# Signing leaves the digest alone.
wasmdump -h /bin/echo.wasm | grep digest > $d/a
wasmdump -h $d/cert.wasm | grep digest > $d/b
cmp $d/a $d/b && echo same digest

echo errors
echo not a module > $d/text
wasmdump $d/text >[2=1]
wasmdep $d/text >[2=1]
echo status $status
read 200 < /lib/test/tiny.wasm > $d/cut.wasm
wasmdump -h $d/cut.wasm /lib/test/tiny.wasm >[2=1] | grep -v '^(size|digest|abi)'
wasmdump $d/missing.wasm >[2=1]
wasmdump >[2=1]
rm -rf $d
