# File name patterns: *, ? and [class], one path element at a time.
load std
d=/tmp/sh-glob
rm -rf $d
mkdir -p $d/l/sub $d/m
cd $d
touch a.c b.c c.h l/x1 l/x2 l/y l/sub/x3 'm/a*' m/ab
echo *.c
echo /bin/sh.was? /b?n/sh.wasm
echo l/x* ./l/x? */x*
echo $d/l/[xy]* l/[0-9a-x]?
echo l/*/x3 l/su?/* */*/*
# No match leaves the word as it is.
echo nomatch* l/z? [ l/x*/y a[b
# Quoted characters, variables and substitutions are not patterns.
echo '*.c' '*'.c *'.c' 'l'/x*
x='*.c'
echo $x $x^'' l/$x
echo `{echo '*.c'}
echo m/'a*' m/a*
# The pattern is the whole word, after concatenation.
echo (a b)^.* (l/x l/y)^*
echo l/*^.bak
# Lists, assignments and redirections are expanded too.
y=*.c
echo $#y $y
for i in l/x* {echo file $i}
echo hi > out
cat < o*
echo more >> o?t
cat out
echo no > *.c
echo no >[2=1] > *.c
# ~ matches a whole word, and / is not special there. Its patterns are
# quoted, or sh would expand them first.
~ l/x1 '*x?' && echo matches
~ '*.c' *.c || echo expanded first
~ abc '[^b]?[a-c]' '*.q' && echo class
~ abc '[^a]*' || echo complement
cd /
rm -r $d
