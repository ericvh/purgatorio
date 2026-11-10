# plumber and plumb: three rules and three ports.
load std
d=/tmp/plumbtest
rm -rf $d
mkdir -p $d
cd $d
echo notes > $d/notes.txt
echo '# plain words go to the editor
kind is text
data matches ''[a-z]+''
plumb to edit

# files named here are viewed, starting the viewer if need be
data matches ''([a-z]+)\.txt''
data isfile $0
data set $file
plumb to view
plumb start cp $file viewed

# images have a port nobody listens on
data matches ''.*\.png''
plumb to image
' > rules
plumber $d/rules
ls /chan | grep plumb

# A program says it reads a port with a control message; messages for
# the port then wait for its reads, one message per read.
plumb -s edit -d plumb start
plumb hello
plumb -s me -a x 1 -a y 'two words' -d edit 'not one word'
read 1000 < /chan/plumb.edit
echo
read 1000 < /chan/plumb.edit
echo

# Nobody reads view yet, so the rule starts its program instead.
plumb notes.txt
while {! test -f viewed} {sleep 0.1}
cat viewed
plumb -s view -d plumb start
plumb notes.txt
read 1000 < /chan/plumb.view
echo

# No reader and no program for image; no rule for the last.
plumb picture.png
plumb 'NO RULE'
echo $status

echo from stdin | plumb -i -d edit
read 1000 < /chan/plumb.edit
plumb -d edit too long for a ten byte read
read 10 < /chan/plumb.edit
read 1000 < /chan/plumb.edit
echo

plumb
plumber $d/nosuch
echo 'data is x' > bad
plumber bad
for p in `{ps | grep plumber | sed 's/^ *([0-9]+) .*/\1/'} {
	kill $p
}
cd /
rm -rf $d
