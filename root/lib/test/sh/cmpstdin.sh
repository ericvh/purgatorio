# cmp reads standard input for a file named - (issue #175).
d=/tmp/sh-cmpstdin
rm -rf $d
mkdir $d
echo same > $d/a
echo other > $d/b
echo same | cmp - $d/a && echo stdin first same
echo same | cmp $d/a - && echo stdin second same
echo same | cmp - $d/b >[2=1]
echo stdin differ $status
cmp - $d/a < $d/b >[2] /dev/null
echo redirected differ $status
rm -r $d
