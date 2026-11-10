# Only space, tab and newline separate words: other white space, such as
# U+00A0, is an ordinary character.
load std
d=/tmp/sh-runes
rm -rf $d
mkdir -p $d
cd $d
z=(a b   c d)
echo $#z $z
x=(e f g)
echo $#x
echo hi > my file
ls
cat my file my?file
y=`{echo one two three}
echo $#y ${quote $y}
for i in ${unquote 'p q r'} {echo $i}
echo ech o=1
echo naïve ☺ 日本語
cd /
rm -r $d
