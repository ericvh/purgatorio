# load std / load expr: Inferno sh-std and sh-expr examples.
load std
load expr
if {~ yes yes} {echo taken} {echo not}
x=0
while {~ $x 0} {
	echo loop
	x=1
}
for i in a b c {echo $i}
echo ${expr 1 2 +}
echo ${expr 3 4 '*'}
fn greet {echo hello $*}
greet there
and {~ a a} {echo and-yes}
if {not {~ a b}} {echo not-yes}
or {~ a b} {echo or-yes}
