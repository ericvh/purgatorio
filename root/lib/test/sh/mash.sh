# mash: the scripts in /lib/test/mash, then flags, standard input and
# loadable modules.
mash /lib/test/mash/lang.mash >[2=1]
mash /lib/test/mash/ctl.mash >[2=1]
mash /lib/test/mash/make.mash >[2=1]
mash /lib/test/mash/misc.mash a 'b c' >[2=1]
echo '# standard input'
echo 'x = 1;
echo $x;
echo (;
echo after' | mash >[2=1]
echo '# -c, -x, -d, -n'
mash -c 'echo a; echo $x; echo b' >[2=1]
mash -x -c 'y = 2; echo $y; echo $y | cat'
mash -d -c 'echo a | cat; x = 1 & if (x) { echo y } else echo z'
mash -n -c 'echo not run'
mash -cecho -c >[2=1]
echo $status
echo '# -e'
mash -e -c 'echo one; cat /nonexistent; echo not reached' >[2=1]
echo $status
mash -e -c 'echo $nope; echo not reached' >[2=1]
echo $status
mash -c 'load make; make nosuch' >[2=1]
echo $status
# Under -e only a pipeline's last command ends mash, and only by failing:
# a prog that traps has its wait message printed, as any prog that dies
# other than by exiting or failing.
mash -e -c 'test -f /nonexistent | echo piped; echo reached' >[2=1]
echo $status
mash -e -c 'sleep 0 | test -f /nonexistent; echo not reached' >[2=1]
echo $status
mash -e -c 'test/debug trap; echo reached' >[2=1] | sed 's/^[0-9]+ /PID /' | sed 's/trap: .*unreachable.*/trap: unreachable/'
echo '# modules'
mash -c 'load mashcase; upper hello world; lower ABC; whatis upper; x = `{upper q r}; echo $x'
mash -c 'load make; whatis make; load tk' >[2=1]
mash -c 'load nosuch; echo still here' >[2=1]
mash -c 'load history; echo extra >> /chan/history; cat /chan/history'
