# runas(1) and per-prog users (docs/auth.md, "Users"). The host owner
# runs commands as bob; they and their children act as bob, and the
# caller keeps its own user. bob cannot change user again, cannot use
# the host owner's #¤, and cannot kill the host owner's progs.
load std
me=`{cat /dev/user}
runas bob cat /dev/user; echo
runas bob sh -c 'cat /dev/user; echo; cat /dev/user; echo'
if {~ `{cat /dev/user} $me} {echo still the host owner}
runas bob ps | grep ' ps$' | sed 's/^ *[0-9]+ +[0-9]+ +[0-9]+ +//' | sed 's/ +[a-z]+ +.*//'

echo refused
runas bob runas alice cat /dev/user
echo $status
runas bob sh -c 'echo alice > /dev/user' >[2=1] | sed 's/^.*: //'
runas bob cat '#¤/ctl' >[2=1] | sed 's/^.*: //'
sleep 100 &
p=$apid
runas bob kill $p >[2=1] | sed 's/^.*: //'
kill $p
wait

echo status
runas bob sh -c 'exit nope'
echo $status
runas bob nonesuch >[2=1]
runas bob >[2=1]
echo done
