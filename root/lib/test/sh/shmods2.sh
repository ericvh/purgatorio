# load file2chan, mload, mpexpr and sexprs: the sh-file2chan(1),
# sh-mload(1), sh-expr(1) and sh-sexprs(1) builtins.
load std mload
d=/tmp/shmods2
rm -rf $d
mkdir -p $d

# expr and mpexpr: postfix arithmetic, 64-bit and arbitrary precision.
load expr
echo ${expr 2 1 -} ${expr 1 2 3 4 + rep} ${expr 7 2 / 7 2 %}
echo ${expr 1 5 seq} / ${expr 5 1 seq}
echo ${expr -r 16 255 16rff +} ${expr -r2 5} ${expr 2r101 8r17 x}
echo ${expr 6 3 and 6 3 or 6 3 xor 5 ~ 5 _}
echo ${expr 1 4 shl -16 2 shr 3 2 gt 3 2 lt 2 2 eq 2 2 neq}
echo ${expr 0 ! 7 not 3 3 ge 3 4 le}
echo ${expr 9223372036854775807 1 +}
ntest 0 || echo zero is false
ntest 2 && echo two is true
{echo ${expr 1 0 /}} >[2=1] | cat
{echo ${expr +}} >[2=1] | cat
{echo ${expr 2 100 xx}} >[2=1] | cat
{echo ${expr -r 99 1}} >[2=1] | cat
mload mp mpexpr
echo ${expr 2 3 +} ${mp expr 2 100 xx}
echo ${mp expr 2 300 xx 3 /}
echo ${mp expr -r 16 2 64 xx 1 -} ${mp expr -r 64 16r010203}
echo ${mp expr 64rAQID 1 +}
echo ${mp expr 4 13 497 expmod 3 7 invert 1000 bits}
echo ${mp expr 2 200 xx _ 3 shr 2 100 xx 1 _ and}
echo ${mp expr 64 rand bits 64 le}
mp ntest 0 || echo mp zero is false
{mp nosuch 1} >[2=1] | cat
{nons expr 1} >[2=1] | cat
munload mp mpexpr
{echo ${mp expr 1}} >[2=1] | cat
munload mp mpexpr >[2=1]
load mpexpr
echo ${expr 3 100 xx}

# mload: the same module in two namespaces.
mload s string
mload r regex
s prefix ab abc && echo s prefix
echo ${s toupper hi} ${r re s l L hello}
munload s string
load string
echo ${toupper ok}

# sexprs: parse and make S-expressions.
load sexprs
x='(snicker "abc" (#03# |YWJj|))'
echo ${els $x}
echo ${textels '(a "b c" d)'}
echo ${text '"a\x41"'} ${text '(a)'}
echo ${mktext 'two words'} ${mktext token} ${mktext 42}
echo ${mklist a '(b c)'} ${mklist}
echo ${mktextlist a 'b c' 1}
echo ${b64 '(a b)'} ${els '{KDE6YTE6Yik=}'}
echo ${els '(12:hello world!(5:inner0:))'}
echo ${text '[text/plain]"hi"'} ${els '([text/plain]"hi")'}
islist $x && echo is a list
islist abc || echo $status
{echo ${els '(a'}} >[2=1] | cat
{echo ${els 'a b'}} >[2=1] | cat
echo '(first 1:a) second' > $d/s
echo '(third (nested "x y"))' >> $d/s
echo 'stop after' >> $d/s
getsexprs {
	if {~ $sexp stop} {raise break}
	echo sexp: $sexp
} < $d/s

# file2chan: a memory file, a logging file and an error.
load file2chan echo
file2chan $d/mem {rreadone $data} {data=${rget data}}
mem=$apid
echo hello there > $d/mem
cat $d/mem
cat $d/mem
file2chan $d/info {rread 'offset '^${rget offset}^' count '^${rget count}} {rerror 'no writes'}
info=$apid
read 9 < $d/info
echo
echo x >[2=1] > $d/info
echo status $status
{file2chan $d/log {} {fetchwdata}} >> $d/logged
log=$apid
echo one >> $d/log
echo two >> $d/log
cat $d/logged
file2chan $d/never {} {}
nv=$apid
cat $d/never >[2=1]
file2chan $d/nodir/x {} {}
echo status $status
{rread nothing} >[2=1] | cat
kill $mem $info $log $nv
rm -rf $d
