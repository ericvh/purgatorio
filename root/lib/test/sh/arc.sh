# ar: make, list, extract, replace, move and delete members of an
# archive, and its errors; ndb/mkhash's table for an ndb file; B sends
# files to the plumber's edit port.
load std
d=/tmp/sh-arc
rm -rf $d
mkdir -p $d/x
cd $d
echo one > one
echo three > three.txt
echo 'a longer file' > two
chmod 644 one two three.txt
touch -t 1000000 one two three.txt
ar rv lib.a one two
ar t lib.a
ar tv lib.a
ar rcv lib.a three.txt
ar t lib.a
xd lib.a > xd.out
sed 4q xd.out
ar p lib.a two
ar pv lib.a one three.txt
# Replace with a newer file only with u; move and insert by position.
echo ONE > one
touch -t 2000000 one
ar ruv lib.a one two
ar mbv one lib.a three.txt
ar t lib.a
ar rav one lib.a /dev/null
ar t lib.a
ar q lib.a two
ar t lib.a
ar dv lib.a two null
ar t lib.a
cd x
ar xo ../lib.a three.txt one
cat one three.txt
ls -l | sed 's/^([^ ]+) .* ([0-9]+) [A-Z][a-z][a-z] .* /\1 \2 /'
cd ..
# Errors.
ar t nosuch.a
echo $status
ar t one
ar tz lib.a
ar t lib.a missing
echo $status
ar x
ar qa one lib.a two
ar r new.a one
ar t new.a

# ndb/mkhash: the table for sys= values.
echo 'sys=alpha ip=10.0.0.1
# a comment
sys=beta ip=10.0.0.2
	sys=gamma
sys=alpha dom=a.example' > db
touch -t 1000 db
ndb/mkhash db sys
xd db.sys
ndb/mkhash db
ndb/mkhash nosuch sys

# B: the editor's port, read here instead of by acme.
echo '# files to the editor
kind is text
data matches ''.*''
plumb to edit' > rules
plumber $d/rules
plumb -s edit -d plumb start
B one -12 x/three.txt
read 1000 < /chan/plumb.edit
echo
read 1000 < /chan/plumb.edit
echo
B
for p in `{ps | grep plumber | sed 's/^ *([0-9]+) .*/\1/'} {
	kill $p
}
# The plumber's files go once it has died: wait for that, or B can still
# open them and find the other end hung up.
while {test -e /chan/plumb.input} {sleep 0.05}
B one
cd /
rm -rf $d
