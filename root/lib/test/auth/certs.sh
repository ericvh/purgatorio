# Signer certificates (docs/auth.md): a server that trusts a signer
# accepts its certificates, and refuses expired, untrusted and revoked
# ones; -A still gets "authentication required".
load std
d=/tmp/auth-certs
rm -rf $d
mkdir -p $d/srv/lib/keys $d/n
u=`{cat /dev/user}
auth/createsignerkey -f $d/sk one > $d/srv/lib/keys/signers
auth/createsignerkey -f $d/sk2 two > $d/signers2
wc -w < $d/srv/lib/keys/signers
auth/mkauthinfo -f $d/sk $u $d/good
auth/mkauthinfo -f $d/sk -e 1 $u $d/old
auth/mkauthinfo -f $d/sk2 $u $d/other
auth/countersigner -f $d/sk $d/other > $d/both
auth/getpk $d/good | wc -c
auth/verify -s $d/srv/lib/keys/signers $d/good > /dev/null && echo verified
auth/verify -s $d/srv/lib/keys/signers $d/old | grep -c '^signer .* trusted$'
auth/verify -s $d/srv/lib/keys/signers -u nobody $d/good > /dev/null
auth/verify -s $d/signers2 $d/good > /dev/null
echo hello > $d/srv/file
styxlisten tcp!127.0.0.1!0 export $d/srv >[2] $d/addr &
pid=$apid
while {! test -s $d/addr} {sleep 0}
srv=tcp!`{sed 's/styxlisten: //' $d/addr}
mount -k $d/good $srv $d/n && cat $d/n/file && unmount $d/n
mount -k $d/old $srv $d/n
mount -k $d/other $srv $d/n
mount -k $d/both $srv $d/n && echo countersigned && unmount $d/n
mount -A $srv $d/n
mount $srv $d/n
auth/getpk $d/good > $d/srv/lib/keys/revoked
mount -k $d/good $srv $d/n
auth/getpk $d/sk > $d/srv/lib/keys/revoked
mount -k $d/good $srv $d/n
rm $d/srv/lib/keys/revoked
import -k $d/good $srv $d/n && cat $d/n/file && unmount $d/n
# The guest factotum holds the key, so a plain mount finds it.
mkdir -p /mnt/factotum
auth/factotum $d/good
grep -c '^key proto=ed25519 ' /mnt/factotum/ctl
mount $srv $d/n && echo factotum && unmount $d/n
echo 'key proto=pass server=s user=bob !password=pw' > /mnt/factotum/ctl
grep -c 'proto=pass server=s user=bob !password?' /mnt/factotum/ctl
grep -c pw /mnt/factotum/ctl
echo 'delkey proto=ed25519' > /mnt/factotum/ctl
mount $srv $d/n
unmount /mnt/factotum
kill $pid
rm -rf $d
