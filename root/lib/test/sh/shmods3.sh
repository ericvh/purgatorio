# More of sh-arg(1), sh-csv(1), sh-mload(1), sh-expr(1), echo and
# sh-file2chan(1): the corners the first two scripts leave out.
load std arg csv echo mload expr file2chan
d=/tmp/shmods3
rm -rf $d
mkdir -p $d

# arg leaves the arguments it did not take in $args as well as $*.
fn opts {arg v {echo -v} - $*; echo args $args; echo star $*}
opts -v a b

# ${csv} of nothing is an empty list, not one empty item.
x=${csv}
echo csv items $#x
x=${csv ''}
echo csv items $#x

# munload fails when the namespace or module is missing.
munload nons expr >[2=1]
echo status $status
mload n expr
munload n string >[2=1]
echo status $status
munload n expr
echo status $status

# A number may be in radix 1, but output cannot be.
echo ${expr 1r0 2 +}
{echo ${expr -r 1 5}} >[2=1] | cat

# rblock holds a read until a write answers it; a close has a tag
# whose fid is the client's. The server's errors go to a file.
{file2chan $d/f {
	if {~ $done 1} {rread ''} {
		pending=$tag
		rblock
		echo ${rget fid} > $d/readfid
	}
} {
	w=${rget data}
	if {~ $w release} {rread $pending released; done=1}
	if {~ $w short} {rwrite $tag 2}
	if {~ $w count} {echo ${rget count}}
} {
	echo ${rget fid} >> $d/closefids
}} >[2] $d/serr
f=$apid
cat $d/f > $d/out &
while {! ls $d/readfid > /dev/null >[2=1]} {sleep 0}
echo -n release > $d/f
wait
cat $d/out
echo
echo -n count > $d/f
cat $d/serr
echo -n short > $d/f
echo status $status
# The server took cat's close before these writes.
grep '^'^`{cat $d/readfid}^'$' $d/closefids > /dev/null && echo closed the read fid

# putrdata and fetchwdata work in a pipeline, as in Inferno.
file2chan $d/g {echo hello world | putrdata} {fetchwdata | tr a-z A-Z > $d/up}
g=$apid
read 5 < $d/g
echo
echo -n shout > $d/g
cat $d/up
echo
kill $f $g
rm -rf $d
