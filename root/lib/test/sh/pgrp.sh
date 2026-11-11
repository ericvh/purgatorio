# pctl newpgrp from load std (issue #116): a sub-shell starts a new
# process group, and kill -g of one member kills the whole group but not
# this shell.
load std expr
d=/tmp/sh-pgrp
rm -rf $d
mkdir $d
@{
	pctl newpgrp
	sleep 100 &
	echo $apid > $d/a
	{sleep 100; echo not killed} &
	echo $apid > $d/b
	echo ${pid} > $d/leader
}
a=`{cat $d/a}
b=`{cat $d/b}
test -d /prog/$a && test -d /prog/$b && echo group running
kill -g $a
echo kill status $status
fn gone {
	n=0
	while {and {test -d /prog/$1} {ntest ${expr 400 $n -}}} {
		sleep 0.05
		n=${expr $n 1 +}
	}
	test -d /prog/$1 || echo $2 gone
}
gone $a first
gone $b second
echo shell survives
kill -g `{cat $d/leader} >[2] /dev/null || echo leader already exited
@{pctl}
echo usage status $status
@{pctl bogus}
echo unknown status $status
@{pctl forkns; bind /lib $d; test -d $d/sh && echo forkns bind}
test -d $d/sh || echo bind stayed in the sub-shell
rm -r $d
