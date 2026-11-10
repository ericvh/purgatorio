# rcmd and cpu on loopback (docs/rcmd.md): auxi/rstyxd behind styxlisten
# runs a command for a client that proved who it is, with the client's
# namespace on /n/client. Standard input, output and error and the exit
# status are carried; a command that ignores its input does not stall
# the session. cpu binds the caller's /dev before the remote one. Then
# with certificates: a signer both ends trust, another they do not, and
# a client with no proof.
load std
d=/tmp/sh-rcmd
rm -rf $d
mkdir -p $d/lib/keys $d/work
u=`{cat /dev/user}
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm >[2] $d/l &
lpid=$apid
while {! test -s $d/l} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l}
echo local > $d/work/f
echo input | rcmd $a 'cat; cat /n/client/tmp/sh-rcmd/work/f; echo remote > /n/client/tmp/sh-rcmd/work/g; echo err >[1=2]; exit bad'
echo status $status
cat $d/work/g
rcmd $a echo 'a  b' 'it''s' c </dev/null
echo status $status
# More input than the pipes hold, to a command that never reads it.
{cat /bin/sh.wasm /bin/sh.wasm | rcmd $a 'cmp /n/client/bin/sh.wasm /bin/sh.wasm && echo same'} >[2] /dev/null
rcmd $a 'cat /bin/sh.wasm' </dev/null > $d/work/copy
cmp $d/work/copy /bin/sh.wasm && echo carried
rcmd $a nosuchcommand </dev/null
echo status $status
cpu $a 'ns | grep client/dev' </dev/null
cpu $a echo 'a  b' </dev/null
rcmd -A $a echo no </dev/null
rcmd -e rc4 $a echo no
rcmd
kill $lpid
# Certificates. The server proves itself to the client's export with -k.
auth/createsignerkey -f $d/sk one > $d/lib/keys/signers
auth/createsignerkey -f $d/sk2 two > /dev/null
auth/mkauthinfo -f $d/sk $u $d/info
auth/mkauthinfo -f $d/sk2 $u $d/other
bind $d/lib /lib
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm -k $d/info >[2] $d/l2 &
lpid=$apid
while {! test -s $d/l2} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l2}
rcmd -k $d/info $a 'cat /n/client/tmp/sh-rcmd/work/f' </dev/null
rcmd -k $d/other $a echo no </dev/null
rcmd -A $a echo no </dev/null
kill $lpid
# A server that asks for no proof, and a client that gives none.
styxlisten -A tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm >[2] $d/l3 &
lpid=$apid
while {! test -s $d/l3} {sleep 0}
a=tcp!`{sed 's/styxlisten: //' $d/l3}
rcmd -A $a echo open </dev/null
kill $lpid
unmount $d/lib /lib
rm -rf $d
