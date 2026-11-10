# load regex: match and ${re}, with regexp(6) syntax.
load std
load regex
fn show {for i in $* {echo '['^$i^']'}}
match '^a.c$' abc && echo match one
match 'b+' abc xbbx && echo match all
match 'b+' abc xyz || echo match some $status
match x && echo match no args
match 'a|b' a b && echo match alternation
echo ${re g '^[a-c]' apple cherry date banana}
echo ${re v '^[a-c]' apple cherry date banana}
echo ${re m 'b+' abbbc}
x=${re m 'z' abc}
echo $#x
show ${re M '([a-z]+)@([a-z]+)(\.org)?' 'mail bob@host now'}
echo ${re mg '[0-9]+' a1b22c333d}
show ${re mg 'x*' abc}
echo ${re s 'o+' 0 foo boo bar}
echo ${re sg o 0 foo boo bar}
echo ${re s '(.)(.)' '\2\1' abcd}
echo ${re s b '<\0\\>' abc}
show ${re sg 'x*' - abc}
echo ${re sg 'a|ab' X abab}
echo ${re g 'colou?r' color colour colr}
echo ${re g '\.wasm$' sh.wasm sh.wasmx x.wasm}
echo ${re g '[^0-9]' 123 1a3}
# Usage and syntax errors.
echo ${re}
echo $status
echo ${re q a b}
echo ${re m a}
echo ${re m a b c}
echo ${re s a}
echo ${re g}
echo ${re g '(' x}
echo ${re g 'a)' x}
echo ${re g '*a' x}
echo ${re g '[z-a]' x}
match
echo $status
match '[' x
echo $status
echo done
