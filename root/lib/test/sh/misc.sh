# Small utilities: tokenize, zeros, uuencode, uudecode, bytes, freq, look,
# tsort, mc, cal, unicode, randpass and p.
d=/tmp/sh-misc
rm -rf $d
mkdir -p $d
tokenize 'a,b,,c d' ', '
tokenize a
zeros 4 2 | xd
zeros -v 41 3
echo
zeros 0
echo 'The quick brown fox' > $d/plain
uuencode remote < $d/plain
uuencode remote < $d/plain | uudecode -p
cd $d
uuencode copy < $d/plain > $d/enc
uudecode $d/enc
cat $d/copy
echo junk | uudecode -p
echo $status
cd /
echo hello | bytes 1 3
echo
echo hello | bytes 1 3 'X\tY'
echo hello | bytes 3 end
bytes 3 1 < /dev/null
echo -n aab | freq
echo -n aab | freq -xc
echo 'apple
apricot
banana
cherry' > $d/words
look ap $d/words
look -x apple $d/words
look -r cherry b $d/words
look zz $d/words
echo $status
look abc $d/nonexistent
echo $status
echo 'a b
b c
a c' | tsort
echo 'x y
y x' | tsort
echo 'one
two
three
four
five' | mc -c 20
cal 1 2000
cal sep 1752
cal 2024 | wc -l
cal 13 2000
unicode 41-49
unicode 263a 41
unicode -t 48 49
unicode -n 42
unicode xyz
randpass | wc -c
randpass 40 | wc -c
randpass 5
echo $status
echo page | p
rm -r $d
