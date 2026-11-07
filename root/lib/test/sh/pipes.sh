# Pipelines, including blocks as stages, and command substitution.
echo a b c | wc -w
echo one two | cat | cat | wc -c
{ echo x; echo y; echo z } | wc -l
words=`{echo alpha beta gamma}
echo $#words $words
n=`{echo a b c d | wc -w}
echo n is $n
echo `{echo nested `{echo deep}}
