# sed(1): addresses and ranges, -n, -e, -f, -g, blocks, labels and
# branches, the hold space, each command, w and r files, and errors.
d=/tmp/sh-sed
rm -rf $d
mkdir -p $d
echo 'line1
line2
line3
line4
line5' > $d/in
echo '# addresses'
sed 2d $d/in
sed '$d' $d/in
sed -n '2,4p' $d/in
sed '2,4!d' $d/in
sed -n '/2/,/4/p' $d/in
sed -n '4,2p' $d/in
sed -n '/3/,$p' $d/in
sed -n '\,5,p' $d/in
sed -n '$=' $d/in
echo '# commands'
sed -n -e 1p -e '$p' $d/in
sed '3q' $d/in
sed -n '2{p;p}' $d/in
sed '1i\
first
$a\
last' $d/in
sed '2,4c\
changed' $d/in
sed '/[24]/c\
even' $d/in
sed -n '2{=;p}' $d/in
sed 'n;d' $d/in
sed '$!N;s/\n/+/' $d/in
sed -n '$!N;P' $d/in
sed '$!N;P;D' $d/in
sed 'y/line/LINE/' $d/in
echo 'tab	here\ end' | sed -n l
echo '# hold space'
sed -n '1!G;h;$p' $d/in
sed -n 'H;${x;s/\n/,/g;p}' $d/in
sed 'x' $d/in
sed '1h;2g' $d/in
echo '# branches'
echo xxaxx | sed ':a;s/x//;ta'
sed '/3/b skip;s/$/ !/;:skip' $d/in
echo 'one two three' | sed 's/two/2/;tdone;s/$/ no/;b;:done;s/$/ yes/'
echo 'one' | sed 's/two/2/;tdone;s/$/ no/;b;:done;s/$/ yes/'
echo '# substitute'
echo foo boo | sed 's/o/0/'
echo foo boo | sed 's/o/0/g'
echo foo boo | sed 's/o/0/3'
echo foo boo | sed -g 's/o/0/'
echo a.c b.c | sed 's/([a-z]+)\.c/<\1>&/g'
echo 'a b' | sed 's/(a) (b)/\2 \1/'
echo 'x&y' | sed 's/&/\&\&/'
echo path/to/f | sed 's,/,\,,g'
echo abc | sed 's/b/\n/'
echo abc | sed 's/x*/-/g'
sed -n 's/line3/LINE3/p' $d/in
sed 's/l//; s/e//' $d/in
echo '# files'
sed -n '/[135]/w '$d/odd $d/in
cat $d/odd
sed -n 's/line2/two/w '$d/two $d/in
cat $d/two
echo inserted > $d/r
sed '2r '$d/r $d/in
sed '2r '$d/nonexistent $d/in
echo '2p' > $d/script
echo 's/line/L/p' >> $d/script
sed -n -f $d/script $d/in
sed -n 1p $d/in $d/in
sed -n '$p' $d/in $d/r
echo '# errors'
sed 'k' $d/in >[2=1]
echo $status
sed '0p' $d/in >[2=1]
sed '1,2q' $d/in >[2=1]
sed 'pq' $d/in >[2=1]
sed '{p' $d/in >[2=1]
sed 'p}' $d/in >[2=1]
sed 'b nowhere' $d/in >[2=1]
sed ':a;:a' $d/in >[2=1]
sed 's/a/b' $d/in >[2=1]
sed 's/*/b/' $d/in >[2=1]
sed 'y/ab/c/' $d/in >[2=1]
sed '//p' $d/in >[2=1]
sed p $d/nonexistent >[2=1]
echo $status
sed -f $d/nonexistent >[2=1]
sed >[2=1]
sed -x p >[2=1]
echo $status
rm -rf $d
