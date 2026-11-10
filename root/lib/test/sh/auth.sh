# Signer certificates on loopback (docs/auth.md): keyfs holds the
# password, getauthinfo gets a certificate from auth/signer after a
# SPAKE2 password check, and an export that trusts the signer accepts it.
# Expired, untrusted and revoked certificates are refused, and -A gets
# "authentication required". The rest of the tools are tested in
# /lib/test/auth, on the emu only.
load std
d=/tmp/sh-auth
rm -rf $d
mkdir -p $d/keys $d/srv/lib/keys $d/n
u=`{cat /dev/user}
auth/createsignerkey -f $d/sk one > $d/srv/lib/keys/signers
auth/createsignerkey -f $d/sk2 two > /dev/null
echo master | auth/keyfs -m $d/keys -C 8,1 $d/keydb
echo secret | auth/changelogin -m $d/keys -C 8,1 $u
cat $d/keys/$u/status
styxlisten tcp!127.0.0.1!0 /bin/auth/signer.wasm -f $d/sk -m $d/keys >[2] $d/sl &
spid=$apid
echo hello > $d/srv/file
styxlisten tcp!127.0.0.1!0 export $d/srv >[2] $d/el &
epid=$apid
while {! test -s $d/sl} {sleep 0}
while {! test -s $d/el} {sleep 0}
signer=tcp!`{sed 's/styxlisten: //' $d/sl}
srv=tcp!`{sed 's/styxlisten: //' $d/el}
echo secret | getauthinfo -s $signer $d/info && echo got
echo wrong | getauthinfo -s $signer $d/bad
mount -k $d/info $srv $d/n && cat $d/n/file && unmount $d/n
auth/mkauthinfo -f $d/sk -e 1 $u $d/old
mount -k $d/old $srv $d/n
auth/mkauthinfo -f $d/sk2 $u $d/other
mount -k $d/other $srv $d/n
auth/getpk $d/info > $d/srv/lib/keys/revoked
mount -k $d/info $srv $d/n
rm $d/srv/lib/keys/revoked
mount -A $srv $d/n
kill $spid $epid
unmount $d/keys
rm -rf $d
