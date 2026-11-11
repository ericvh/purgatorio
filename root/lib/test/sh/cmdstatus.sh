# The exit statuses of cmp, diff and mkdir, as their Inferno pages give
# them (issue #115).
d=/tmp/sh-cmdstatus
rm -rf $d
mkdir $d
echo same > $d/a
echo same > $d/b
echo other > $d/c
cmp $d/a $d/b && echo cmp same
cmp $d/a $d/c >[2] /dev/null
echo cmp differ $status
cmp $d/a $d/a2 >[2] /dev/null
echo cmp missing $status
echo 'same
more' > $d/prefix
cmp $d/a $d/prefix >[2] /dev/null
echo cmp eof $status
diff $d/a $d/b && echo diff same
diff $d/a $d/c > /dev/null
echo diff some $status
mkdir $d/a >[2] /dev/null
echo mkdir existing $status
mkdir -p $d && echo mkdir -p existing
rm -r $d
