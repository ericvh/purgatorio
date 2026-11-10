# load string: the sh-string(1) builtins, and the list builtins of load std.
load std
load string
fn show {for i in $* {echo '['^$i^']'}}
prefix ab abc && echo prefix yes
prefix abd abc || echo prefix no $status
in c a-z && echo in range
in C a-z || echo in not-range $status
in C '^a-z' && echo in negated
in '' abc || echo in empty $status
show ${splitl a.b.c .}
show ${splitl abc .}
show ${splitr a.b.c .}
show ${splitr abc .}
show ${drop '  x y' ' '}
show ${take abc123 a-z}
show ${splitstrl a::b::c ::}
show ${splitstrl abc ::}
show ${splitstrr a::b::c ::}
show ${splitstrr abc ::}
echo ${tolower HeLLo} ${toupper HeLLo}
echo ${len héllo} ${alen héllo} ${len ''}
show ${slice 1 3 héllo}
show ${slice 3 end hello}
show ${slice 4 2 hello}
show ${slice 2 99 hello}
x=${fields : a::b:}
echo $#x
show $x
show ${padl 5 ab}
show ${padr 5 ab}
show ${padl 4 a b}
show ${padr 2 abcdef}
show ${padl 3}
# The list builtins load std adds.
l=(one two three)
echo ${hd $l} / ${tl $l} / ${index 2 $l}
y=${index 9 $l}
echo $#y
q=${quote a 'b c' ''}
echo $#q $q
show ${unquote $q}
# Usage errors.
echo ${len}
echo $status
echo ${len a b}
echo ${slice x 2 abc}
echo ${slice 1 -1 abc}
echo ${padl x a}
echo ${splitl a}
echo ${fields a}
prefix a
echo $status
in a b c
echo ${index x a}
echo done
