# Text tools: sort, uniq, comm, tr, fmt, strings, sha1sum and sum.
d=/tmp/sh-text
rm -rf $d
mkdir -p $d
echo 'pear
apple
fig
apple
banana' > $d/fruit
sort $d/fruit
sort -r $d/fruit
echo '10
-3
2
+7
x
007' | sort -n
echo '10
2
1' | sort -nr
sort $d/nonexistent
echo $status
sort -q
sort $d/fruit | uniq
sort $d/fruit | uniq -c
sort $d/fruit | uniq -d
sort $d/fruit | uniq -u
uniq $d/nonexistent
echo 'a
b
d' > $d/one
echo 'b
c
d' > $d/two
comm $d/one $d/two
comm -12 $d/one $d/two
comm -3 $d/one $d/two
echo b | comm -23 - $d/two
comm $d/one
echo $status
echo Hello World | tr a-z A-Z
echo hello | tr -d l
echo 'aaa   bbb' | tr -s ' a'
echo 'one, two; three' | tr -cs a-z '
'
echo abc | tr ab x
echo abc | tr '\141' '\x42'
echo 'a-b' | tr - _
echo '  indented
one two three four five six seven eight nine ten
eleven twelve

next paragraph' | fmt -l 20
echo 'a b
c d' | fmt -j -i 4
echo 'x' > $d/f1
echo 'y' > $d/f2
fmt $d/f1 $d/f2
echo 'ab
printable string
x' > $d/bin
strings $d/bin
echo -n abc | sha1sum
sha1sum $d/one
sha1sum $d/nonexistent
echo $status
sum $d/one
