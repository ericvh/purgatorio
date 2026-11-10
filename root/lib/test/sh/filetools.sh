# File tools: basename, cleanname, touch, chmod, chgrp, test, newer,
# read, xargs, time and dd.
d=/tmp/sh-filetools
rm -rf $d
mkdir -p $d
basename /a/b/c.b
basename /a/b/c.b .b
basename c.b c.b
basename -d /a/b/c.b
basename -d c.b
cleanname /a//b/./c/../d a/../.. '#c/../cons' ''
cleanname -d /usr/me ../x /abs
touch $d/new
test -f $d/new && echo created
touch -t 100 $d/old
touch -t 200 $d/new
newer $d/new $d/old && echo newer
newer $d/old $d/new
echo $status
newer $d/nonexistent $d/old
echo $status
touch -c $d/missing
echo $status
test -e $d/missing || echo not created
echo data > $d/f
chmod 644 $d/f
ls -l $d/f | sed 's/ .*//'
chmod +x $d/f
ls -l $d/f | sed 's/ .*//'
test -x $d/f && echo executable
chmod go-rx $d/f
ls -l $d/f | sed 's/ .*//'
chmod a=r $d/f
ls -l $d/f | sed 's/ .*//'
chmod u+w $d/f
chmod 8r640 $d/f
ls -l $d/f | sed 's/ .*//'
chmod q $d/f
chmod 644 $d/nonexistent
echo $status
chgrp -u `{cat /dev/user} $d/f && echo chgrp ok
test -d $d && echo dir
test -d $d/f || echo not dir
test -s $d/f && echo nonempty
test -s $d/new || echo empty
test -r $d/f && echo readable
test abc = abc && echo eq
test abc != abd && echo ne
test -n '' || echo empty string
test -z '' && echo zero
test 10 -gt 9 -a 3 -le 3 && echo arith
test 1 -eq 2 -o ! 1 -eq 2 && echo or-not
test '(' 1 -lt 2 ')' && echo paren
test '' || echo false string
test word && echo true string
test $d/old -ot $d/new && echo older
test $d/new -nt $d/old && echo newer
test $d/old -older 1d && echo old file
test 1 -foo 2
echo $status
test '(' 1
echo $status
test
echo $status
echo hello > $d/r
read 3 < $d/r
echo
read -o 2 2 < $d/r
echo
read -e -3 < $d/r
read < /dev/null
echo $status
echo 'a b
c

d' | xargs echo x
echo $d/f $d/nonexistent | xargs cat
echo $status
xargs nonexistent < /dev/null
time echo timed >[2] /dev/null
time nonexistent >[2] /dev/null
echo $status
echo -n hello world | dd -of $d/dd -bs 4 -count 2
cat $d/dd
echo
echo -n hello | dd -of $d/dd -conv ucase,swab
cat $d/dd
echo
echo -n abcdefgh | dd -of $d/dd -ibs 2 -skip 1 -count 2
cat $d/dd
echo
echo -n abc | dd -of $d/dd -ibs 2 -conv sync
xd $d/dd
echo 'ab
cdefg' | dd -of $d/dd -cbs 4 -conv block
cat $d/dd
echo
dd -if $d/dd -cbs 4 -conv unblock
dd -foo 1
echo $status
rm -r $d
