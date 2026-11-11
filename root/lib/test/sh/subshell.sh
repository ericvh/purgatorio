# @{...} runs a block in a sub-shell (issue #116): cd, variables, load
# and bind inside it do not reach this shell, and its status does.
load std
d=/tmp/sh-subshell
rm -rf $d
mkdir -p $d/dir $d/over $d/mnt
echo over > $d/over/f
cd $d
x=outer
y:=local
fn f {echo f in sub}
@{cd dir; pwd; x=inner; y=changed; echo $x $y; echo set in sub > $d/dir/g}
pwd
echo $x $y
echo `{cat /env/x}
sh -c 'echo child sees $x'
cat dir/g
@{f}
@{z=new; echo z is $z}
echo z is $#z
cat /env/z >[2] /dev/null || echo no /env/z
@{load expr; echo expr in sub ${expr 1 2 +}}
ntest 1 >[2] /dev/null || echo no expr here
@{bind $d/over $d/mnt; cat $d/mnt/f}
ls $d/mnt
ns | grep -c $d/mnt || echo not bound here
@{bind $d/over $d/mnt; ns | grep -c $d/mnt}
@{exit three}
echo status $status
@{cat /nonexistent} >[2] /dev/null
echo status $status
@{echo ok} && echo and ran
@{exit bad} || echo or ran
! @{exit bad} && echo negated
@ echo plain words
@{@{x=deeper; cd /; pwd}; echo $x; pwd}
@{echo args $*}
@{echo out; echo err >[1=2]} >[2] $d/err | sed 's/^/piped /'
cat $d/err
@{echo backgrounded > $d/bg} &
wait
cat $d/bg
cd /
rm -r $d
