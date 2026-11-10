# ed: addresses, editing commands, global commands, files and undo.
d=/tmp/sh-ed
rm -rf $d
mkdir -p $d
echo 'one
two
three
four
five' > $d/f
echo '2p
$p
/fo/p
?t?p
-1,+1n
,p
=
.=
2;+1p
' | ed $d/f
echo 'a
after one
.
1,2p
3i
before three
.
,n
2,3d
,p
1c
ONE
.
1,$s/e/E/g
,p
w
q' | ed - $d/f
cat $d/f
echo 's/E/e/2p
,s/(o)(.)/\2\1/p
1s/ONE/&-&/p
s/-/\
/
,p
u
,p
q
Q' | ed - $d/f
echo 'g/e/s/e/X/p
v/X/p
g/X/.-1,.p
1m$
,p
1,2t0
,p
2,3j
,p
1kx
$
''xp
w '$d'/out
e '$d'/out
,l
r '$d'/f
=
f
!echo shell
q' | ed - $d/f
echo 'xyz
99p
/nomatch/
s/zzz/y/
k1
q' | ed - $d/f
ed - $d/nonexistent < /dev/null
echo 'a
	tab\back
.
,l
w' | ed -o
rm -r $d
