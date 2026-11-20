# Redirections.
d=/tmp/sh-redir
rm -rf $d
mkdir $d
echo first > $d/f
echo second >> $d/f
cat $d/f
wc -l < $d/f
cat /nonexistent >[2] $d/err
cat $d/err
{ echo to stderr >[1=2] } >[2] $d/err2
cat $d/err2
cat /nonexistent >[2=]
echo closed $status
{ echo out; cat /nonexistent } > $d/both >[2=1]
cat $d/both
# A shell started for a block keeps the fds opened around it.
{echo kept | {cat >[1=5]}} >[5] $d/fd5
cat $d/fd5
{echo twice | {{cat >[1=5]} | cat}} >[5] $d/fd5
cat $d/fd5
{echo sub | @{cat >[1=5]}} >[5] $d/fd5
cat $d/fd5
rm -r $d
