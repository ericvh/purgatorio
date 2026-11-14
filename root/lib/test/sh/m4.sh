# m4: the macro processor of m4(1), its built-ins, quoting, diversions,
# include, syscmd, the options and the errors.
d=/tmp/sh-m4
rm -rf $d
mkdir -p $d
echo '# text, names and plain definitions
hello, world
define(`x'',`y'')x x1 1x _x (x) inferno
define(`p'',`<`$0''|$1|$2|$3>'')dnl
p(a,b) p(  a,
 b ,c ) p((a,b),c) p(`a,b'',c) p p ()
define(`c'',`a,b'')define(`q'',`[$2]'')q(c)
define(`s'',`$$1$9$'')s(x)
define(`self'',`self'')self define(`zero'',`$0'')zero
define(`v'',`1'')define(`w'',`v'')w `w'' ``w'''' `'' `nest `deep'' x''
ifdef(`inferno'',yes,no) ifdef(`nope'',yes,no) ifdef(`x'',only)|
' > $d/basic.m4
m4 $d/basic.m4
echo '-- builtins'
echo 'len(abc) len(`'') len(`a,b'') len(é)
index(abcdcd,cd) index(abc,x) index(abc,)
substr(hello,1,3) substr(hello,3) substr(hello,9)| substr(hello,-2,2)
translit(hello,lo,01) translit(hello,el) translit(`a-b'',-,_)
incr(41) incr(-1) incr(x) incr( 7)
ifelse(a,a,1) ifelse(a,b,1,2) ifelse(a,b,1,c,c,3,4) ifelse(a,b,1,c,d,3,4) ifelse(a,b,1)|
define(`t'',1)undefine(`t'')t define(`t'')[t]
copydef(`define'',`mydef'')mydef(`u'',2)u copydef(`u'',`u2'')u2 copydef(`none'',`u3'')[u3]
define(`len'',`gone'')len(abc)
abc dnl this goes
def
divnum
' | m4
echo '-- eval'
echo 'eval(1+2*3) eval((1+2)*3) eval(7/2) eval(-7%3) eval(2**3**2) eval(-2**2)
eval(1<<4|1) eval(-16>>2) eval(6&3^1) eval(1+1==2&&3>2) eval(0||!0) eval(~0)
eval(3=3) eval(2!=2) eval(2<=2) eval(1>2) eval(+-+5) eval(9223372036854775807+1)
eval(` 1 + 2 '')
' | m4
echo '-- recursion'
echo 'define(`fact'',`ifelse($1,0,1,`eval($1*fact(eval($1-1)))'')'')dnl
fact(0) fact(5) fact(10)
define(`reverse'',`ifelse($1,,,`reverse(substr($1,1))`''substr($1,0,1)'')'')dnl
reverse(abcdef)
define(`count'',`ifelse($1,$2,,`$1 count(incr($1),$2)'')'')count(1,6)
' | m4
echo '-- quotes and comments'
echo 'define(`x'',`y'')# x is not expanded `here
x
changecom(/*,*/)/* x
 x */ x /x # x
changecom(@)@ x
x changecom # x
changequote([,])[`x''] [[n]] [x]
changequote(|)|x|
changequote`x''
changequote(<<,>>)<<a>> changequote
' | m4
echo '-- diversions'
echo 'divert(2)two
divert(1)one
divert(9)nine
divert`''zero
divnum
divert(-1)gone
divert(10)also gone divnum
divert(3)divnum
divert(0)undivert(2)dnl
divert(4)undivert(1)dnl
undivert(0,5,11)dnl
divert`''dnl
end
' | m4
echo 'divert(1)x
divert(2)y
divert(0)undivert
divert(1)a
divert(2)b
divert(2)undivert`''dnl
divert(0)undivert(2)dnl
' | m4
echo '-- include'
echo 'define(`inc'',`included'')inc
' > $d/hdr.m4
echo 'before include('$d'/hdr.m4)inc
sinclude('$d'/none.m4)after
define(`f'',`'$d'/hdr.m4'')include(f,f)dnl
' | m4
echo '-- syscmd'
echo 'one
syscmd(`echo two'')three
syscmd(`echo four > '$d'/four'')include('$d'/four)dnl
syscmd(`echo m4 subcommand >[1=2]'')dnl
' | m4 >[2=1]
echo '-- maketemp'
echo 'define(`t'',maketemp('$d'/fooXXXXX))dnl
ifelse(substr(t,0,len('$d'/foo.)),'$d'/foo.,prefix ok)
substr(t,eval(len(t)-1)) translit(substr(t,eval(len(t)-11),10),0123456789,dddddddddd)
syscmd(`echo > ''t)define(`t2'',maketemp('$d'/fooXXX))dnl
substr(t2,eval(len(t2)-1)) ifelse(substr(t,0,eval(len(t)-1)),substr(t2,0,eval(len(t2)-1)),same base)
' | m4
echo '-- errprint and dumpdef'
echo 'errprint(`a message'',two)errprint()errprint
define(`dd'',`a $1'')define(`d e'',x)copydef(`len'',`ln'')dumpdef(`dd'',`ln'',`d e'',`missing'')
changequote([,])dumpdef([dd])
' | m4 >[2=1]
echo 'undefine(`inferno'')dumpdef' | m4 >[2=1]
echo '-- options'
echo 'A B C inferno define' > $d/opts.m4
m4 -DA -DB=b '-QC=`B''' -Uinferno $d/opts.m4
m4 -p m4_ -DA=a $d/opts.m4
echo 'm4_define(`B'',bee)B define(`B'',x)B m4_len(abc) len(abc)' | m4 -pm4_
echo 'λdefine(`B'',bee)B λlen(abc)' | m4 -pλ
m4 -Ddefine=x $d/opts.m4 >[2=1]
echo exit $status
m4 -pm4_ -Ddefine=x $d/opts.m4
m4 -Udefine $d/opts.m4
echo 'define(`x'',`$1-$2'')x(a,b) len(x)' | m4 -t >[2=1]
echo 'a' > $d/two.m4
m4 $d/opts.m4 $d/two.m4
echo 'define(B,two)' > $d/defb.m4
m4 $d/defb.m4 $d/opts.m4
m4 -x >[2=1]
echo exit $status
m4 -D >[2=1]
echo exit $status
echo '-- errors'
m4 $d/missing.m4 >[2=1]
echo exit $status
echo 'ok
include('$d'/missing.m4)' | m4 >[2=1]
echo exit $status
echo 'sinclude('$d'/hdr.m4, '$d'/missing.m4)' | m4 >[2=1]
echo exit $status
echo 'start
define(`x'',' | m4 >[2=1]
echo exit $status
echo 'start
`unterminated
' > $d/str.m4
m4 $d/str.m4 >[2=1]
echo exit $status
echo 'eval(1+)' | m4 >[2=1]
echo 'eval(1 2 3)' | m4 >[2=1]
echo 'eval(`(1'')' | m4 >[2=1]
echo 'eval(1/0)' | m4 >[2=1]
echo 'eval(5%0)' | m4 >[2=1]
echo 'eval(2**-1)' | m4 >[2=1]
echo exit $status
echo 'divert(1)kept only on success
eval(x)' | m4 >[2=1]
echo -n '# comment at the end' | m4 >[2=1]
echo exit $status
rm -r $d
