# Redirections that fail, to files on user-level servers and elsewhere:
# the command does not run, the shell says why on its own standard error,
# and its files still work afterwards.
load std
d=/tmp/sh-redirfail
rm -rf $d
mkdir -p $d/src $d/m $d/t
cd $d
echo hello > src/a
tar cf t.tar src
# A read-only export, to this system only: -A, as nothing is trusted.
mount {export -A -r src} m
echo x > m/a
echo x >> m/a
echo x > m/new
echo after export
{echo x; echo y} > m/a
if {~ a a} {echo x} > m/a
y=`{echo x > m/a}
echo $#y
cat m/a
unmount m
# A read-only server.
tarfs t.tar t
echo x > t/src/a
echo x > t/src/new
cat < t/src/nope
echo after tarfs
unmount t
# A server that allows writing.
memfs m
echo x > m/no/such
echo x > m
echo x > m/f > m/no/such
ls m
echo ok > m/f
cat m/f
{echo one; echo x > m/no/such; echo two; cat < m/nope; echo three} > m/out
cat m/out
unmount m
# The error does not go to a file the failed command had opened.
{echo x} >[2] errs > /no/such/file
x=1 >[2] errs > /no/such/file
cat errs
echo $#x
cd /
rm -r $d
echo done
