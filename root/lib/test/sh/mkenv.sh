# mk reads a list that sh exports as the same list (issue #170).
d=/tmp/sh-mkenv
rm -rf $d
mkdir $d
cd $d
echo 'all:VQ:
	echo $#WORDS
	load std; echo second ${index 2 $WORDS}
n:VQ: $WORDS
	echo prereqs $prereq
%:VQ:
	echo made $target
' > mkfile
WORDS=(one 'two words' three)
mk all
mk n
WORDS=single
mk all
cd /
rm -r $d
