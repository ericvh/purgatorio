# sh -e, and mk's parallel recipes ($NPROC) and recipes that stop at
# their first failing command.
sh -e -c 'echo one; cat /nonexistent; echo two'
echo status $status
sh -e -c 'echo last; cat /nonexistent >[2] /dev/null'
echo status $status
sh -e -c 'cat /nonexistent >[2] /dev/null || echo or; echo after or'
sh -e -c 'load std; if {cat /nonexistent >[2] /dev/null} {echo yes} {echo no}; echo after if'
sh -e -c '{cat /nonexistent >[2] /dev/null; echo block} ; echo after block'
echo status $status
sh -e -c '{echo in sub; cat /nonexistent >[2] /dev/null; echo not here} | cat; echo not here'
echo status $status
sh -c 'cat /nonexistent >[2] /dev/null; echo without -e'

d=/tmp/mkpar
rm -rf $d
mkdir $d
cd $d
# a waits for b, so it finishes only if b runs at the same time.
echo 'all:V: a b
	echo all done
a:V:
	load std
	for i in 1 2 3 {test -e bflag || sleep 1}
	test -e bflag
	echo a slot $nproc > alog
b:V:
	echo b slot $nproc > blog
	touch bflag
' > mkfile
mk NPROC=2
cat alog blog
rm -f alog blog bflag
@{NPROC=2; mk}
cat alog blog

# With one job slot, a runs alone and fails.
rm -f alog blog bflag
mk
echo status $status

# A recipe stops at its first failing command, and mk waits for the
# other running recipe before it gives up.
echo 'all:V: f g
f:V:
	echo f one > flog; cat /nonexistent >[2] /dev/null; echo f two >> flog
g:V:
	sleep 1; echo g ran > glog
' > mk2
mk -f mk2 NPROC=2
echo status $status
cat flog glog

# -k carries on with the other targets.
echo 'all:V: f h
f:V:
	cat /nonexistent >[2] /dev/null; echo f two
h:V:
	echo h ran > hlog
' > mk3
mk -k -f mk3 NPROC=3
echo status $status
cat hlog
cd /
rm -rf $d
