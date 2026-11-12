# The install/ package and replica tools: create, inst, info, wrap2list,
# updatelog and applylog, on small trees in /tmp.
d=/tmp/installtest
rm -rf $d
mkdir -p $d/src/app/lib $d/dest
cd $d
echo one > src/app/one
echo two > src/app/lib/two
chmod 664 src/app/one src/app/lib/two
chmod 775 src/app src/app/lib
touch -t 1000000 src/app/one src/app/lib/two src/app src/app/lib
echo 'app
	+' > proto

# A base package, its header and contents, and what info says of it.
install/create -x -p proto -s src hello
install/create -t 5000 -N inferno -G inf -d 'Hello world' -p proto -s src hello
install/inst -h 5000
install/info 5000
install/create -o -t 5000 -N inferno -G inf -d 'Hello world' -p proto -s src hello > out
cmp out 5000 && echo same on stdout

# Install it; again, with a file changed locally.
install/inst -t -r dest 5000
cat dest/app/one dest/app/lib/two
cat dest/wrap/hello/5000/md5sum
install/info -r dest hello
echo local > dest/app/one
install/inst -r dest 5000
cat dest/app/one
install/inst -F -r dest 5000
cat dest/app/one

# An update: a changed file, a new one and a removed one.
echo TWO > src/app/lib/two
echo three > src/app/three
rm src/app/one
install/create -u -t 6000 -r dest -d fixes -s src hello
install/inst -h 6000
install/inst -r dest 6000
ls dest/app dest/app/lib
cat dest/wrap/hello/6000/update dest/wrap/hello/6000/remove
install/info -r dest hello
install/wrap2list -r dest | sed 's/	.*//'
install/wrap2list -r dest | wc -l
install/inst -r dest nosuch
install/info -r dest nosuch

# A replica: log the server tree, apply it, change it, update the log.
mkdir server client
echo a > server/a
mkdir server/dir
echo b > server/dir/b
chmod 664 server/a server/dir/b
chmod 775 server/dir
touch -t 2000000 server/a server/dir/b server/dir
install/updatelog -t 100 0 -r server log > entries
sed 's/^([0-9]* [0-9]* . [^ ]* [^ ]* [0-7]*).*/\1/' entries
install/applylog clientlog client server < entries
cat client/a client/dir/b
wc -l clientlog
echo A > server/a
touch -t 3000000 server/a
rm server/dir/b
install/updatelog -t 200 0 -r server clientlog > entries2
wc -l entries2
install/applylog -n clientlog client server < entries2
install/applylog -T stamp clientlog client server < entries2
cat client/a
ls client/dir
cat stamp
# A conflict: the replica's file changed on its own.
echo mine > client/a
echo AA > server/a
touch -t 4000000 server/a
install/updatelog -t 300 0 -r server clientlog > entries3
install/applylog -T stamp clientlog client server < entries3
cat client/a
install/applylog -e -T stamp clientlog client server < entries3
echo $status
install/applylog -s -T stamp clientlog client server < entries3
cat client/a
install/updatelog -c -t 400 0 -r server clientlog | sed 's/^([0-9]* [0-9]* . [^ ]* [^ ]* [0-7]*).*/\1/'
cd /
rm -rf $d
