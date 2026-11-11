# The server proves itself after the client (docs/auth.md, "Server
# authentication"). mount -S and rcmd -S require that proof and check it
# against what this namespace trusts as a server: /lib/keys/servers,
# /lib/keys/signers and the test key. A server that asks for no proof,
# or proves itself with a certificate from a signer nobody here trusts,
# is refused. export -e and rstyxd -e refuse to go on in the clear.
load std
d=/tmp/sh-srvauth
rm -rf $d
mkdir -p $d/n $d/empty
u=`{cat /dev/user}
auth/createsignerkey -f $d/sk nobody > $d/signers
auth/mkauthinfo -f $d/sk $u $d/impostor
styxlisten tcp!127.0.0.1!0 export / >[2] $d/el &
epid=$apid
styxlisten tcp!127.0.0.1!0 export -A / >[2] $d/al &
apid2=$apid
styxlisten tcp!127.0.0.1!0 export -e / >[2] $d/xl &
xpid=$apid
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm >[2] $d/rl &
rpid=$apid
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm -k $d/impostor >[2] $d/il &
ipid=$apid
for f in el al xl rl il {while {! test -s $d/$f} {sleep 0}}
e=tcp!`{sed 's/styxlisten: //' $d/el}
a=tcp!`{sed 's/styxlisten: //' $d/al}
x=tcp!`{sed 's/styxlisten: //' $d/xl}
r=tcp!`{sed 's/styxlisten: //' $d/rl}
i=tcp!`{sed 's/styxlisten: //' $d/il}
mount -S $e $d/n && wc -c < $d/n/lib/test/auth.key.pub && unmount $d/n
mount -S $a $d/n
mount -A -S $e $d/n
mount $x $d/n && echo encrypted && unmount $d/n
rcmd -S $r 'echo proved; wc -c < /n/client/lib/test/auth.key.pub' </dev/null
rcmd -S $i echo no </dev/null
echo status $status
kill $epid $apid2 $xpid $rpid $ipid
# With no key anywhere the export and rstyxd cannot encrypt, and -e
# makes them refuse instead of serving in the clear.
bind $d/empty /lib/test
styxlisten tcp!127.0.0.1!0 export -e / >[2] $d/xl2 &
xpid=$apid
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm -e >[2] $d/rl2 &
rpid=$apid
for f in xl2 rl2 {while {! test -s $d/$f} {sleep 0}}
x=tcp!`{sed 's/styxlisten: //' $d/xl2}
r=tcp!`{sed 's/styxlisten: //' $d/rl2}
mount $x $d/n
rcmd $r echo no </dev/null
kill $xpid $rpid
unmount /lib/test
rm -rf $d
