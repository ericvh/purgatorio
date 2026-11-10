# mk: rules, meta-rules, variables, attributes and flags on a small
# project in /tmp. File times are set with touch -t so that the results do
# not depend on the clock.
d=/tmp/mktest
rm -rf $d
mkdir -p $d/dir
cd $d
echo hello > a.txt
echo world > b.txt
touch -t 100 a.txt b.txt
echo 'NAMES=a b
UP=${NAMES:%=%.up}
# the first rule is the default target
all:V: out
out: $UP
	cat $prereq > $target
%.up: %.txt
	tr a-z A-Z < $stem.txt > $target
clean:V:
	rm -f out $UP
' > mkfile
mk
cat out
touch -t 200 a.up b.up out
mk
mk out
touch -t 300 a.txt
mk -n
mk -e -n a.up
mk -n -a
mk -n -w b.txt
mk -t
mk clean
ls
mk nosuch
echo $status
mk -f nofile
echo $status

# & stops at . and /, % does not; regular-expression rules; Q.
echo 'all:V: x.lo dir/y.lo q.r
&.lo:Q:	&.txt
	echo amp $stem $target
dir/%.lo:
	echo pct $stem
([a-z])\.r:R:	\1.txt
	echo re $stem1 $prereq
' > mk2
touch q.txt x.txt
mk -f mk2

# D deletes a failed target, E carries on, N touches, -k keeps going.
echo 'f:D: a.txt
	echo partial > $target; cat /nonexistent
g:E: a.txt
	cat /nonexistent >[2] /dev/null
h:N: a.txt
' > mk3
mk -f mk3 f
ls f
mk -f mk3 -k f g h
echo $status
ls h

# Variables: command-line overrides, U, backquotes, includes, MKFLAGS.
echo 'V=from-include' > inc
echo 'one two' > words
echo '<inc
<|echo W=piped
X=first
X=second
L=`{cat words}
Q=U=hidden
show:V:
	echo $V $W $X $L $#L
	echo Q $Q flags $MKFLAGS
	echo args $MKARGS
' > mk4
mk -f mk4
mk -f mk4 X=cmd show

# P: the program decides whether the target is out of date.
echo same > p1
echo same > p2
echo 'p1:Pcmp >[2] /dev/null: p2
	echo copy $prereq
' > mk5
mk -f mk5
echo other > p2
mk -f mk5

# A missing intermediate is not remade when what needs it is newer.
echo 'fin: mid
	cp mid fin
mid: a.txt
	cp a.txt mid
' > mk6
mk -f mk6
rm mid
touch -t 400 fin
mk -f mk6
mk -f mk6 -i -n

# Errors.
echo 'a: b
	echo one
a: c
	echo two
' > mk7
touch b c
mk -f mk7
echo 'loop: loop2
loop2: loop
' > mk8
mk -f mk8
echo 'stray words' > mk9
mk -f mk9
mk -f mk7 -z
cd /
rm -rf $d
