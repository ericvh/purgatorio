# The signer's network side (docs/auth.md): keyfs holds the passwords,
# getauthinfo gets a certificate after a SPAKE2 password check, and the
# certificate mounts a server that trusts the signer. auth/passwd
# changes the password through keysrv.
load std
d=/tmp/auth-login
rm -rf $d
mkdir -p $d/keys $d/srv/lib/keys $d/n
u=`{cat /dev/user}
auth/createsignerkey -f $d/sk > $d/srv/lib/keys/signers
echo master | auth/keyfs -m $d/keys -C 8,1 $d/keydb
echo secret | auth/changelogin -m $d/keys -C 8,1 $u
ls $d/keys | wc -l
cat $d/keys/$u/status
grep -c '^argon2id 8 1 1 ' $d/keys/$u/secret
styxlisten tcp!127.0.0.1!0 /bin/auth/signer.wasm -f $d/sk -m $d/keys >[2] $d/sl &
spid=$apid
styxlisten tcp!127.0.0.1!0 /bin/auth/keysrv.wasm -f $d/sk -m $d/keys >[2] $d/kl &
kpid=$apid
echo hello > $d/srv/file
styxlisten tcp!127.0.0.1!0 export $d/srv >[2] $d/el &
epid=$apid
while {! test -s $d/sl} {sleep 0}
while {! test -s $d/kl} {sleep 0}
while {! test -s $d/el} {sleep 0}
signer=tcp!`{sed 's/styxlisten: //' $d/sl}
keysrv=tcp!`{sed 's/styxlisten: //' $d/kl}
srv=tcp!`{sed 's/styxlisten: //' $d/el}
echo secret | getauthinfo -s $signer $d/info && echo got
auth/verify -s $d/srv/lib/keys/signers -u $u $d/info > /dev/null && echo verified
mount -k $d/info $srv $d/n && cat $d/n/file && unmount $d/n
echo wrong | getauthinfo -s $signer $d/info2
echo secret | getauthinfo -u nobody -s $signer $d/info2
{echo secret; echo newpw; echo newpw} | auth/passwd -s $keysrv && echo changed
echo secret | getauthinfo -s $signer $d/info2
echo newpw | getauthinfo -s $signer $d/info2 && echo got again
echo disabled > $d/keys/$u/status
echo newpw | getauthinfo -s $signer $d/info2
echo ok > $d/keys/$u/status
echo 1 > $d/keys/$u/expire
echo newpw | getauthinfo -s $signer $d/info2
kill $spid $kpid $epid
# The key file survives keyfs and needs the master password.
unmount $d/keys
echo wrong | auth/keyfs -m $d/keys $d/keydb
echo master | auth/keyfs -m $d/keys $d/keydb
cat $d/keys/$u/expire
unmount $d/keys
rm -rf $d
