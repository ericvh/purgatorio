# The redirections of a command apply to a function it calls and to the
# blocks std's if, while, for, and, or and not run.
load std
d=/tmp/sh-fnredir
rm -rf $d
mkdir $d
fn f {
	echo out
	cat $d/missing
}
f > $d/o >[2] $d/e
echo function; cat $d/o; cat $d/e
if {echo cond} {echo then} > $d/o
echo if; cat $d/o
for i in a b {echo $i} > $d/o
echo for; cat $d/o
n=()
while {! ~ $#n 2} {n=($n x); echo $#n} > $d/o
echo while; cat $d/o
and {echo one} {echo two} > $d/o
or {echo three} {echo four} >> $d/o
echo and or; cat $d/o
not {echo five} > $d/o
echo not $status; cat $d/o
f | wc -l
rm -r $d
