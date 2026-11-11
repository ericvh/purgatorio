# The conversation after an authenticated mount is encrypted
# (docs/auth.md, "Mount handshake, version 3"). A file several messages
# long goes both ways through an authenticated mount on loopback, with
# the public test key, and comes back whole; the same through -A against
# an export that does not authenticate, which stays in the clear. The
# test keys are bound on /lib/keys: the stock root trusts no key.
load std
bind /lib/test/keys /lib/keys
d=/tmp/sh-styxcrypt
rm -rf $d
mkdir -p $d/srv $d/n
cp /bin/sh.wasm $d/srv/big
styxlisten tcp!127.0.0.1!0 export $d/srv >[2] $d/el &
epid=$apid
styxlisten tcp!127.0.0.1!0 export -A $d/srv >[2] $d/al &
apid2=$apid
while {! test -s $d/el} {sleep 0}
while {! test -s $d/al} {sleep 0}
srv=tcp!`{sed 's/styxlisten: //' $d/el}
clear=tcp!`{sed 's/styxlisten: //' $d/al}
mount -c $srv $d/n && echo mounted
cmp $d/srv/big $d/n/big && echo read whole
cp $d/srv/big $d/n/back
cmp $d/srv/big $d/srv/back && echo wrote whole
unmount $d/n
mount -A $clear $d/n && echo mounted clear
cmp $d/srv/big $d/n/big && echo read clear
unmount $d/n
mount -A $srv $d/n
kill $epid $apid2
unmount /lib/keys
rm -rf $d
