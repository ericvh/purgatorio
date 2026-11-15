# CPU limits (docs/abi.md, "Memory, prog and CPU limits"): a prog past its
# own cpulimit is killed, and so are the progs it starts after the limit
# was set; a limit cannot be raised; a domain past its cpulimit has every
# member killed, a sleeping one too.
load std
@{
	echo cpulimit 200ms > /prog/${pid}/ctl
	sh -c 'load std; while {~ a a} {}'
	echo spin $status
}
echo cpulimit 2 > /prog/${pid}/ctl
echo cpulimit 3 > /prog/${pid}/ctl >[2] /dev/null
echo raise $status
echo cpulimit 1x > /prog/${pid}/ctl >[2] /dev/null
echo bad $status
@{
	pctl newpgrp
	bind '#pdomain' /prog
	echo domain cpulimit 300ms > /prog/${pid}/ctl
	sleep 100 &
	sh -c 'load std; while {~ a a} {}'
	echo not reached
}
echo domain $status
echo sleepers left `{ps | grep -c ' sleep$'}
