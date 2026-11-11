# grep's exit statuses (issue #115): no match, -s, -l, -L, an unopenable
# file and a bad pattern, in if, && and ||.
load std
d=/tmp/sh-grepstatus
rm -rf $d
mkdir $d
echo 'one apple
two pears' > $d/a
echo 'three plums' > $d/b
grep apple $d/a && echo matched
echo status $status
grep banana $d/a || echo no banana
echo status $status
grep -s apple $d/a $d/b && echo quiet match
grep -s banana $d/a $d/b
echo -s status $status
if {grep -s plum $d/b} {echo if took plum} {echo not reached}
if {grep -s kiwi $d/b} {echo not reached} {echo if took else}
if {! grep -s kiwi $d/a $d/b} {echo negated}
grep -v apple $d/a
grep -vs . $d/b || echo every line has a character
grep -n pears $d/a
grep -n p $d/a $d/b
grep -c p $d/a
grep -c kiwi $d/a || echo counted none
grep -l p $d/a $d/b
grep -l pears $d/a $d/b
grep -L pears $d/a $d/b
echo -L status $status
grep -L kiwi $d/a $d/b
echo -L none status $status
grep -i APPLE $d/a
echo two | grep -l two
grep apple $d/missing $d/a >[2] $d/err
echo missing file status $status
cat $d/err
grep apple $d/missing >[2] /dev/null
echo only missing status $status
grep 'a(b' $d/a >[2] $d/err
echo bad pattern status $status
cat $d/err
grep >[2] $d/err
echo usage status $status
cat $d/err
grep -x apple $d/a >[2] /dev/null
echo bad option status $status
rm -r $d
