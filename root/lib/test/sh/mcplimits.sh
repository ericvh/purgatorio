# mcpd stage 5: a session is a prog domain with limits. Past -M a program
# fails and the result says it was the memory limit; a fork bomb stops at
# -P with no free procs; a second session goes on meanwhile. A prog that
# left its group, and whose parent exited, dies with the session. The
# session has its own #s and no #¤.
load std expr
d=/tmp/mcplimits.${pid}
rm -rf $d
mkdir $d
fn init {
	echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"sh","version":"0"}}}'
}
fn call {
	echo '{"jsonrpc":"2.0","id":'^$1^',"method":"tools/call","params":{"name":"run","arguments":{"command":"'^$2^'"}}}'
}
fn has {
	grep '"id":'^$1^',' $d/$2 | grep -s $3 && echo $1 $4
}
# A host file2chan file a session must not see.
load file2chan
file2chan '#s/mcplimits'^${pid} {rread host} {}
{
	init
	call 2 'load std; fn grow {grow $1^$1}; grow xxxxxxxxxxxxxxxx'
	call 3 'echo after the memory'
	call 4 'load std; fn bomb {bomb & bomb &}; bomb; sleep 1'
	echo '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"reset","arguments":{}}}'
	call 6 'echo after the bomb'
	call 7 'load std; @{pctl newpgrp; sleep 1000 & echo stray $apid}'
} | mcpd -M 16m -P 16 > $d/a &
bombed=$apid
{
	init
	call 2 'sleep 2; echo other session'
	call 3 'ls ''#s''; cat ''#s/mcplimits'^${pid}^'''; ls ''#¤'''
	call 4 'load file2chan; file2chan /tmp/f {rreadone mine} {}; cat /tmp/f; ls ''#s'' | wc -l'
} | mcpd > $d/b
has 2 b 'other session' 'other session ran'
has 3 b '#s/mcplimits[0-9]*: file does not exist' 'host srv file not there'
has 3 b '#¤: mount/attach disallowed' 'no factotum'
has 4 b '"stdout":"mine *1\\n"' 'own srv file'
n=0
while {and {test -d /prog/$bombed} {ntest ${expr 600 $n -}}} {
	sleep 0.05
	n=${expr $n 1 +}
}
has 2 a 'out of memory: the session''s programs may hold 16 MiB' 'memory limit'
has 2 a 'memory allocation of' 'allocation failed'
has 3 a 'after the memory' 'served on'
has 4 a 'no free procs: the session may run at most 16 programs' 'prog limit'
has 5 a '"reset":true' 'reset'
has 6 a 'after the bomb' 'served on'
stray=`{grep '"id":7,' $d/a | sed 's/.*stray //' | sed 's/[^0-9].*//'}
~ $#stray 0 && echo no stray pid
n=0
while {and {test -d /prog/$stray} {ntest ${expr 400 $n -}}} {
	sleep 0.05
	n=${expr $n 1 +}
}
test -d /prog/$stray || echo stray gone
rm -r $d
