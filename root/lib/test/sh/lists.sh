# List variables keep their words in every sub-shell (issue #163): the
# /env file holds the words quoted and separated by spaces, as Inferno's
# sh writes them, and a sub-shell reads them back as a list.
load std
d=/tmp/sh-lists
rm -rf $d
mkdir $d
l=(a 'b c' d)
echo '# backquote'
x=`{echo ${hd $l}}
echo $x
echo `{echo ${index 2 $l}}
echo `{echo $#l}
echo '# pipeline'
{echo $#l; echo ${index 2 $l}} | cat
echo '# background'
{echo $#l ${index 2 $l} > $d/bg} &
wait
cat $d/bg
echo '# @'
@{echo $#l ${index 3 $l}}
echo '# run'
echo 'load std; echo $#l ${tl $l}' > $d/script
run $d/script
echo '# sh -c'
sh -c 'load std; echo $#l ${index 2 $l}'
sh $d/script
echo '# one word with a blank'
w='one word'
cat /env/w
echo
sh -c 'echo $#w'
echo '# empty words'
e=('')
sh -c 'echo $#e'
e2=('' x '')
sh -c 'echo $#e2'
q=('it''s' '$x' 'a=b')
cat /env/q
echo
sh -c 'load std; echo $#q ${index 1 $q}'
echo '# empty list'
n=()
ls /env/n >[2] /dev/null || echo n unset
sh -c 'echo $#n'
echo '# plain text from another program'
echo -n plain > /env/p
sh -c 'echo $#p $p'
echo '# locals reach sub-shells but not /env'
loc:=(x y z)
echo `{echo $#loc}
{echo $#loc} | cat
ls /env/loc >[2] /dev/null || echo loc not exported
sh -c 'echo $#loc'
rm -r $d
