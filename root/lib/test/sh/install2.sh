# install/wdiff, install/wfind, install/mergelog and fs log, on a small
# package and replica log in /tmp.
load std
d=/tmp/sh-install2
rm -rf $d
mkdir -p $d/src/app/lib $d/dest/install/hello
cd $d
echo one > src/app/one
echo two > src/app/lib/two
echo keep > src/app/keep
chmod 664 src/app/one src/app/lib/two src/app/keep
chmod 775 src/app src/app/lib
touch -t 1000000 src/app/one src/app/lib/two src/app/keep src/app src/app/lib
echo 'app
	+' > proto
install/create -t 5000 -N inferno -G inf -d 'Hello world' -p proto -s src hello
install/inst -t -r dest 5000
cp 5000 dest/install/hello/5000

# wdiff: nothing changed, then a changed file and a removed one.
install/wdiff -r dest 5000
echo ONE > dest/app/one
rm dest/app/lib/two
install/wdiff -l -r dest 5000
install/wdiff -r dest 5000
# An update covers the files it holds, unless -p.
echo TWO > src/app/lib/two
touch -t 1000000 src/app/lib/two
install/create -u -t 6000 -r dest -d fixes -s src hello
install/inst -F -r dest 6000
install/wdiff -l -r dest 5000
install/wdiff -l -p -r dest 5000
install/wdiff -r dest nosuch
install/wdiff -r dest proto
install/wdiff

# wfind: which packages hold a file.
install/wfind -r dest one two
install/wfind -r dest -p hello keep
install/wfind -r dest nothing
install/wfind

# mergelog: an old log and a newer section become one.
echo '100 0 a app - 20000000775 inferno inf 10 0
100 1 a app/a - 664 inferno inf 10 1
100 2 a app/b - 664 inferno inf 10 2' > old
echo '200 0 c app/a - 664 inferno inf 20 5
200 1 m app/b - 600 inferno inf 10 2
200 2 a app/c - 664 inferno inf 20 3
200 3 d app/a - 664 inferno inf 20 5
200 4 c app/x - 664 inferno inf 20 3' | install/mergelog old
install/mergelog nosuch < /dev/null
echo 'bad entry' | install/mergelog old
install/mergelog

# fs log: log entries for a tree, as a fresh replica log.
fs log -u inferno -g inf {walk src/app} | sed 's/^[0-9]+ //'
cd /
rm -rf $d
