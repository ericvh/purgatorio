# Servers per host (docs/auth.md). A signer this namespace trusts has
# certified two servers: one for 127.0.0.1, the host dialled here, and
# one for elsewhere. Under -S only the first proves it is the server
# dialled; the second is an impostor, though its certificate is good. A
# key in /lib/keys/servers proves the host its line names, and no other.
# rcmd gives /n/client without a second handshake only to a server that
# proved it is the host dialled; the impostor has to prove a user this
# namespace trusts, and its certificate names no such user.
load std
d=/tmp/sh-srvhost
rm -rf $d
mkdir -p $d/n $d/keys
echo client file > $d/f
auth/createsignerkey -f $d/sk hosts > $d/keys/signers
cp /lib/test/keys/users $d/keys/users
auth/mkauthinfo -f $d/sk 127.0.0.1 $d/here
auth/mkauthinfo -f $d/sk elsewhere $d/there
bind $d/keys /lib/keys
styxlisten tcp!127.0.0.1!0 export -k $d/here / >[2] $d/hl &
hpid=$apid
styxlisten tcp!127.0.0.1!0 export -k $d/there / >[2] $d/tl &
tpid=$apid
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm -k $d/here >[2] $d/rl &
rpid=$apid
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm -k $d/there >[2] $d/il &
ipid=$apid
for f in hl tl rl il {while {! test -s $d/$f} {sleep 0}}
h=tcp!`{sed 's/styxlisten: //' $d/hl}
t=tcp!`{sed 's/styxlisten: //' $d/tl}
r=tcp!`{sed 's/styxlisten: //' $d/rl}
i=tcp!`{sed 's/styxlisten: //' $d/il}
mount -S $h $d/n && cat $d/n/tmp/sh-srvhost/f && unmount $d/n
mount -S $t $d/n
import -S $t $d/n
mount $t $d/n && echo not required && unmount $d/n
# A key listed for this host proves it; listed for another, it does not.
echo 127.0.0.1 `{auth/getpk $d/there} > $d/keys/servers
mount -S $t $d/n && echo listed && unmount $d/n
echo elsewhere `{auth/getpk $d/there} > $d/keys/servers
mount -S $t $d/n
rm $d/keys/servers
# rcmd: the real server gets /n/client; the impostor is refused under -S,
# and without -S its own mount of /n/client is.
rcmd -S $r cat /n/client/tmp/sh-srvhost/f </dev/null
rcmd $r cat /n/client/tmp/sh-srvhost/f </dev/null
rcmd -S $i echo no </dev/null
rcmd $i cat /n/client/tmp/sh-srvhost/f </dev/null
echo status $status
kill $hpid $tpid $rpid $ipid
unmount /lib/keys
rm -rf $d
