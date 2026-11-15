# cmp(1)'s options and messages: the first difference by character, -L
# with its line, -l with every difference, -s with none, offsets in
# decimal, hex and octal, the end of one file (reported even with -s, as
# Inferno's cmp does), an offset that does not parse, and usage.
d=/tmp/sh-cmpopts
rm -rf $d
mkdir $d
echo 'one
two
three' > $d/a
echo 'one
twX
thrYe' > $d/b
cmp $d/a $d/b
echo status $status
cmp -L $d/a $d/b
cmp -l $d/a $d/b
echo status $status
cmp -s $d/a $d/b
echo status $status
echo xxone > $d/c
cmp $d/a $d/c 0 2
cmp $d/a $d/c 0 0x2 >[2=1]
cmp $d/a $d/c 0 02 >[2=1]
echo 'one' > $d/short
cmp -s $d/a $d/short
echo status $status
cmp $d/a $d/a 0 nonsense && echo an offset that does not parse is 0
cmp -x $d/a $d/b >[2=1]
cmp $d/a >[2=1]
echo status $status
rm -r $d
